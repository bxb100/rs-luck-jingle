use anyhow::{Context, Result, anyhow};
use axum::body::{Body, Bytes};
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Json, Router};
use jiff::Timestamp;
use rs_luck_jingle::discovery::{
    PrinterCandidate, discover_printers, select_printer, write_printer_candidates,
};
use rs_luck_jingle::markdown::{MarkdownImageFetcher, parse_markdown};
use rs_luck_jingle::protocol::Density;
use rs_luck_jingle::render::{load_image_bytes, render_text, stack_vertical};
use rs_luck_jingle::session::{PrintFailure, PrinterSession, SessionConfig};
use rs_luck_jingle::transport::{RfcommTransport, Transport};
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::env;
use std::future::IntoFuture;
use std::io::{self, BufRead, IsTerminal, Write};
use std::net::TcpListener;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const QUEUE_CAPACITY: usize = 100;
const DEDUPE_CACHE_CAPACITY: usize = 1_024;
const MAX_MARKDOWN_BODY_BYTES: usize = 64 * 1024;
const DEFAULT_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(12);
const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(30);
const HTTP_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
const IDEMPOTENCY_KEY_HEADER: &str = "Idempotency-Key";
const REQUEST_ID_HEADER: &str = "X-Request-ID";

static NEXT_JOB_ID: AtomicU64 = AtomicU64::new(1);

struct PrintJob {
    job_id: String,
    text: String,
    image_urls: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct PrintContent {
    text: String,
    image_urls: Vec<String>,
}

struct AppState {
    queue: Mutex<Option<SyncSender<PrintJob>>>,
    dedupe: Mutex<DedupeCache>,
}

impl AppState {
    fn close_queue(&self) {
        lock_mutex(&self.queue).take();
    }
}

/// Locks a mutex, recovering the guarded value even if a prior panic left
/// the mutex poisoned. A poisoned lock here would otherwise cascade into a
/// panic on every future request, so we deliberately keep serving requests
/// with whatever state survived instead of taking the whole process down.
fn lock_mutex<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum DedupeKey {
    GithubDelivery(String),
    PrintIdempotency(String),
}

struct DedupeCache {
    capacity: usize,
    order: VecDeque<DedupeKey>,
    entries: HashMap<DedupeKey, String>,
}

impl DedupeCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            order: VecDeque::with_capacity(capacity),
            entries: HashMap::with_capacity(capacity),
        }
    }

    fn get(&self, key: &DedupeKey) -> Option<&str> {
        self.entries.get(key).map(String::as_str)
    }

    fn insert(&mut self, key: DedupeKey, job_id: String) {
        if self.capacity == 0 {
            return;
        }

        if self.order.len() == self.capacity
            && let Some(expired) = self.order.pop_front()
        {
            self.entries.remove(&expired);
        }
        self.order.push_back(key.clone());
        self.entries.insert(key, job_id);
    }
}

struct RuntimeConfig {
    printer_address: Option<String>,
    rfcomm_channel: Option<u8>,
    discovery_timeout: Duration,
    session: SessionConfig,
    bind_address: String,
}

impl RuntimeConfig {
    fn from_env() -> Result<Self> {
        let printer_address = optional_env("LUCK_JINGLE_PRINTER_ADDRESS")?;
        let rfcomm_channel = parse_optional_env("LUCK_JINGLE_RFCOMM_CHANNEL")?;
        let discovery_timeout_secs = parse_env(
            "LUCK_JINGLE_DISCOVERY_TIMEOUT_SECS",
            DEFAULT_DISCOVERY_TIMEOUT.as_secs(),
        )?;
        if discovery_timeout_secs == 0 {
            return Err(anyhow!(
                "LUCK_JINGLE_DISCOVERY_TIMEOUT_SECS must be greater than zero"
            ));
        }
        let density_level = parse_env("LUCK_JINGLE_DENSITY", u8::from(Density::Normal))?;
        let density = Density::try_from(density_level)?;
        let feed_dots = parse_env(
            "LUCK_JINGLE_FEED_DOTS",
            rs_luck_jingle::protocol::DEFAULT_FEED_DOTS,
        )?;
        let bind_address =
            env::var("LUCK_JINGLE_BIND_ADDRESS").unwrap_or_else(|_| "0.0.0.0:5444".to_owned());

        Ok(Self {
            printer_address,
            rfcomm_channel,
            discovery_timeout: Duration::from_secs(discovery_timeout_secs),
            session: SessionConfig {
                density,
                feed_dots,
                ..SessionConfig::default()
            },
            bind_address,
        })
    }
}

fn optional_env(name: &str) -> Result<Option<String>> {
    match env::var(name) {
        Ok(value) if value.trim().is_empty() => Ok(None),
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {name}")),
    }
}

fn parse_optional_env<T>(name: &str) -> Result<Option<T>>
where
    T: FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    optional_env(name)?
        .map(|value| {
            value
                .parse()
                .with_context(|| format!("invalid value for {name}"))
        })
        .transpose()
}

fn parse_env<T>(name: &str, default: T) -> Result<T>
where
    T: FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    match env::var(name) {
        Ok(value) => value
            .parse()
            .with_context(|| format!("invalid value for {name}")),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error).with_context(|| format!("failed to read {name}")),
    }
}

fn main() -> io::Result<()> {
    #[cfg(target_os = "macos")]
    if let Some(result) = rs_luck_jingle::macos_bluetooth::run_helper_if_requested() {
        return result.map_err(io::Error::other);
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> io::Result<()> {
    env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));
    let config = RuntimeConfig::from_env().map_err(io::Error::other)?;
    let listener = bind_http_listener(&config.bind_address)?;
    let printer_address = resolve_printer_address(config.printer_address, config.discovery_timeout)
        .await
        .map_err(io::Error::other)?;
    let transport = build_transport(&printer_address, config.rfcomm_channel)
        .await
        .map_err(io::Error::other)?;
    let session_config = config.session;
    let session = tokio::task::spawn_blocking(move || {
        let mut session = PrinterSession::new(transport, session_config);
        session
            .initialize()
            .context("failed to initialize selected printer")?;
        Ok::<_, anyhow::Error>(session)
    })
    .await
    .map_err(io::Error::other)?
    .map_err(io::Error::other)?;
    let (queue, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
    let print_worker = tokio::task::spawn_blocking(move || {
        run_print_worker(receiver, session, HEALTH_CHECK_INTERVAL)
    });

    let state = Arc::new(AppState {
        queue: Mutex::new(Some(queue)),
        dedupe: Mutex::new(DedupeCache::new(DEDUPE_CACHE_CAPACITY)),
    });
    let listener = tokio::net::TcpListener::from_std(listener)?;
    let axum_addr = listener.local_addr()?;

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let mut server = Box::pin(
        axum::serve(listener, build_router(state.clone()))
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .into_future(),
    );
    log::info!(
        "HTTP server listening on {} and printing to {}",
        axum_addr,
        printer_address
    );

    let server_result = tokio::select! {
        result = &mut server => result,
        _ = shutdown_signal() => {
            let _ = shutdown_tx.send(());
            match tokio::time::timeout(HTTP_SHUTDOWN_TIMEOUT, &mut server).await {
                Ok(result) => result,
                Err(_) => {
                    log::warn!(
                        "HTTP graceful shutdown timed out after {HTTP_SHUTDOWN_TIMEOUT:?}"
                    );
                    Ok(())
                }
            }
        }
    };
    state.close_queue();
    drop(server);
    if let Err(error) = print_worker.await {
        if server_result.is_ok() {
            return Err(io::Error::other(error));
        }
        log::error!("print worker failed while HTTP server was stopping: {error}");
    }
    server_result
}

fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", any(not_found).get(index).head(not_found))
        .route(
            "/print",
            any(not_found)
                .post(print_markdown)
                .layer(DefaultBodyLimit::max(MAX_MARKDOWN_BODY_BYTES)),
        )
        .route("/github-webhooks", any(not_found).post(github_webhooks))
        .with_state(state)
}

async fn not_found() -> StatusCode {
    StatusCode::NOT_FOUND
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            log::error!("failed to listen for Ctrl-C: {error}");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                log::error!("failed to listen for SIGTERM: {error}");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

fn bind_http_listener(bind_address: &str) -> io::Result<TcpListener> {
    let listener = TcpListener::bind(bind_address)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

async fn resolve_printer_address(
    configured_address: Option<String>,
    discovery_timeout: Duration,
) -> Result<String> {
    if let Some(address) = configured_address {
        log::info!("using configured printer address: {address}");
        return Ok(address);
    }

    let candidates = discover_printers(discovery_timeout)
        .await
        .context("failed to discover D1X printers")?;
    let selected = tokio::task::spawn_blocking(move || {
        let stdin = io::stdin();
        let stdout = io::stdout();
        select_discovered_printer(
            &candidates,
            &mut stdin.lock(),
            &mut stdout.lock(),
            stdin.is_terminal(),
        )
    })
    .await
    .context("printer selection task failed")??;

    log::info!(
        "selected printer: name={}, address={}",
        selected.name,
        selected.address
    );
    Ok(selected.address)
}

fn select_discovered_printer<R, W>(
    candidates: &[PrinterCandidate],
    input: &mut R,
    output: &mut W,
    is_interactive: bool,
) -> Result<PrinterCandidate>
where
    R: BufRead,
    W: Write,
{
    if candidates.len() > 1 && !is_interactive {
        write_printer_candidates(candidates, output)?;
        return Err(anyhow!(
            "multiple printers were discovered without an interactive terminal; set LUCK_JINGLE_PRINTER_ADDRESS to one of the listed MAC addresses"
        ));
    }

    select_printer(candidates, input, output)
}

async fn build_transport(address: &str, channel: Option<u8>) -> Result<RfcommTransport> {
    if let Some(channel) = channel {
        log::warn!(
            "using explicitly configured RFCOMM channel {channel}; automatic SPP discovery is disabled"
        );
        return RfcommTransport::new(address, channel);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        return RfcommTransport::from_profile(address)
            .await
            .context("failed to connect selected printer through the SPP profile");
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(anyhow!(
            "automatic Bluetooth Classic connection is unsupported on {}",
            std::env::consts::OS
        ))
    }
}

fn run_print_worker<T>(
    receiver: Receiver<PrintJob>,
    mut session: PrinterSession<T>,
    health_check_interval: Duration,
) where
    T: Transport,
{
    let mut next_health_check = Instant::now() + health_check_interval;
    loop {
        let remaining = next_health_check.saturating_duration_since(Instant::now());
        let job = match receiver.recv_timeout(remaining) {
            Ok(job) => job,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                next_health_check = check_printer_health(&mut session, health_check_interval)
                    + health_check_interval;
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };

        if Instant::now() >= next_health_check {
            next_health_check =
                check_printer_health(&mut session, health_check_interval) + health_check_interval;
        }

        let image = match render_print_job(&job) {
            Ok(image) => image,
            Err(error) => {
                log::error!(
                    "print failed before raster generation: job={}, retry_safe=true, error={error:#}",
                    job.job_id
                );
                continue;
            }
        };

        let result = session.print(&image);
        next_health_check = Instant::now() + health_check_interval;
        match result {
            Ok(outcome) => log::info!(
                "print completed: job={}, raster_bytes={}",
                job.job_id,
                outcome.raster_bytes
            ),
            Err(PrintFailure::RetrySafe(error)) => {
                log::error!(
                    "print failed before raster transmission: job={}, retry_safe=true, error={error:#}",
                    job.job_id
                );
            }
            Err(PrintFailure::OutcomeUnknown(error)) => {
                log::error!(
                    "print outcome is unknown: job={}, automatic_retry=false, manual_verification=true, error={error:#}",
                    job.job_id
                );
            }
        }
    }

    if let Err(error) = session.disconnect() {
        log::warn!("failed to disconnect printer while stopping print worker: {error:#}");
    }
}

fn check_printer_health<T>(
    session: &mut PrinterSession<T>,
    health_check_interval: Duration,
) -> Instant
where
    T: Transport,
{
    let result = session.health_check();
    let completed_at = Instant::now();
    match result {
        Ok(outcome) if outcome.reconnected => log::info!(
            "printer health check restored connection: status={:?}",
            outcome.status
        ),
        Ok(outcome) => log::debug!(
            "printer health check succeeded: status={:?}",
            outcome.status
        ),
        Err(error) => log::warn!(
            "printer health check failed; next retry in {health_check_interval:?}: {error:#}"
        ),
    }
    completed_at
}

fn render_print_job(job: &PrintJob) -> Result<image::RgbImage> {
    let mut sections = Vec::with_capacity(job.image_urls.len() + 1);
    if !job.text.is_empty() {
        sections.push(render_text(&job.text).context("failed to render print text")?);
    }

    if !job.image_urls.is_empty() {
        let fetcher = MarkdownImageFetcher::from_env();
        for (image_index, url) in job.image_urls.iter().enumerate() {
            let image = match &fetcher {
                Ok(fetcher) => fetcher
                    .fetch(url)
                    .with_context(|| {
                        format!("failed to download Markdown image {}", image_index + 1)
                    })
                    .and_then(|bytes| {
                        load_image_bytes(&bytes).with_context(|| {
                            format!("failed to decode Markdown image {}", image_index + 1)
                        })
                    }),
                Err(error) => Err(anyhow!(
                    "failed to configure Markdown image downloads: {error:#}"
                )),
            };
            let image = match image {
                Ok(image) => image,
                Err(error) => {
                    log::warn!(
                        "Markdown image omitted: job={}, image={}, error={error:#}",
                        job.job_id,
                        image_index + 1
                    );
                    render_text("[image unavailable]")
                        .context("failed to render Markdown image fallback")?
                }
            };
            sections.push(image);
        }
    }

    stack_vertical(&sections).context("failed to compose printable content")
}

async fn index() -> Response {
    body_response(StatusCode::OK, "Ok")
}

async fn print_markdown(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !has_markdown_content_type(&headers) {
        return body_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type must be text/markdown or text/markdown; charset=utf-8",
        );
    }

    let idempotency_key = match parse_idempotency_key(&headers) {
        Ok(key) => key,
        Err(message) => return body_response(StatusCode::BAD_REQUEST, message),
    };
    let markdown = match std::str::from_utf8(&body) {
        Ok(markdown) => markdown,
        Err(_) => {
            return body_response(StatusCode::BAD_REQUEST, "Markdown body must be valid UTF-8");
        }
    };
    let content = match parse_markdown(markdown) {
        Ok(content) => content,
        Err(error) => {
            log::warn!("invalid Markdown print request: {error:#}");
            return body_response(StatusCode::BAD_REQUEST, "Markdown body is invalid");
        }
    };
    if content.text.is_empty() && content.image_urls.is_empty() {
        return body_response(
            StatusCode::BAD_REQUEST,
            "Markdown body has no printable content",
        );
    }

    let job_id = next_job_id();
    let key = idempotency_key.map(DedupeKey::PrintIdempotency);
    let job = PrintJob {
        job_id: job_id.clone(),
        text: content.text,
        image_urls: content.image_urls,
    };
    match enqueue_print_job(&state, key, job) {
        Ok(accepted_job_id) => accepted_response(&accepted_job_id),
        Err(error) => {
            log::error!("failed to enqueue Markdown print job: {error}");
            unavailable_response()
        }
    }
}

async fn github_webhooks(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    hook: Result<Json<GithubWebhook>, JsonRejection>,
) -> Response {
    let Json(hook) = match hook {
        Ok(hook) => hook,
        Err(error) => {
            let status = if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
                StatusCode::PAYLOAD_TOO_LARGE
            } else {
                StatusCode::BAD_REQUEST
            };
            return body_response(status, error.body_text());
        }
    };
    let event = match header_value(&headers, "X-GitHub-Event") {
        Some(event) => event,
        None => return body_response(StatusCode::BAD_REQUEST, "missing X-GitHub-Event"),
    };
    let delivery_id = header_value(&headers, "X-GitHub-Delivery")
        .unwrap_or("missing-delivery-id")
        .to_owned();
    let content = match build_message(event, &hook) {
        Ok(Some(content)) => content,
        Ok(None) => return StatusCode::NO_CONTENT.into_response(),
        Err(error) => return body_response(StatusCode::BAD_REQUEST, error.to_string()),
    };

    let job_id = next_job_id();
    let job = PrintJob {
        job_id: job_id.clone(),
        text: content.text,
        image_urls: content.image_urls,
    };
    let key = DedupeKey::GithubDelivery(delivery_id);
    match enqueue_print_job(&state, Some(key), job) {
        Ok(accepted_job_id) => accepted_response(&accepted_job_id),
        Err(error) => {
            log::error!("failed to enqueue GitHub print job: {error}");
            unavailable_response()
        }
    }
}

fn enqueue_print_job(
    state: &AppState,
    dedupe_key: Option<DedupeKey>,
    job: PrintJob,
) -> Result<String, mpsc::TrySendError<PrintJob>> {
    let job_id = job.job_id.clone();
    let queue = lock_mutex(&state.queue);
    let Some(queue) = queue.as_ref() else {
        return Err(mpsc::TrySendError::Disconnected(job));
    };
    let Some(dedupe_key) = dedupe_key else {
        queue.try_send(job)?;
        return Ok(job_id);
    };

    let mut cache = lock_mutex(&state.dedupe);
    if let Some(existing_job_id) = cache.get(&dedupe_key) {
        return Ok(existing_job_id.to_owned());
    }

    queue.try_send(job)?;
    cache.insert(dedupe_key, job_id.clone());
    Ok(job_id)
}

fn accepted_response(job_id: &str) -> Response {
    (StatusCode::ACCEPTED, [(REQUEST_ID_HEADER, job_id)], ()).into_response()
}

fn unavailable_response() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::RETRY_AFTER, "1")],
        (),
    )
        .into_response()
}

fn body_response(status: StatusCode, body: impl Into<Body>) -> Response {
    Response::builder()
        .status(status)
        .body(body.into())
        .unwrap_or_else(|error| {
            log::error!("failed to build HTTP response: {error}");
            (StatusCode::INTERNAL_SERVER_ERROR, ()).into_response()
        })
}

fn next_job_id() -> String {
    let sequence = NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed);
    format!(
        "job-{}-{}-{sequence}",
        std::process::id(),
        Timestamp::now().as_microsecond()
    )
}

fn has_markdown_content_type(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(header::CONTENT_TYPE).iter();
    let Some(value) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }
    let Ok(value) = value.to_str() else {
        return false;
    };

    let mut parts = value.split(';');
    if !parts
        .next()
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("text/markdown"))
    {
        return false;
    }

    let mut charset_seen = false;
    for parameter in parts {
        let Some((name, value)) = parameter.split_once('=') else {
            return false;
        };
        if charset_seen || !name.trim().eq_ignore_ascii_case("charset") {
            return false;
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .unwrap_or(value);
        if !value.eq_ignore_ascii_case("utf-8") {
            return false;
        }
        charset_seen = true;
    }

    true
}

fn parse_idempotency_key(headers: &HeaderMap) -> Result<Option<String>, &'static str> {
    let mut values = headers.get_all(IDEMPOTENCY_KEY_HEADER).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err("Idempotency-Key must appear at most once");
    }

    let bytes = value.as_bytes();
    if !(1..=128).contains(&bytes.len()) || !bytes.iter().all(|byte| (0x21..=0x7e).contains(byte)) {
        return Err("Idempotency-Key must contain 1 to 128 visible ASCII characters");
    }

    Ok(Some(
        std::str::from_utf8(bytes)
            .map_err(|_| "Idempotency-Key must contain 1 to 128 visible ASCII characters")?
            .to_owned(),
    ))
}

fn header_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

fn build_message(event: &str, hook: &GithubWebhook) -> Result<Option<PrintContent>> {
    let now = Timestamp::now().strftime("%Y-%m-%d %H:%M:%S");
    match event {
        "issues" if hook.action.as_deref() != Some("opened") => Ok(None),
        "issues" => {
            let issue = hook
                .issue
                .as_ref()
                .context("issues payload is missing issue")?;
            let body = parse_markdown(issue.body.as_deref().unwrap_or_default())?;
            Ok(Some(PrintContent {
                text: format!(
                    "{now}\nREPO: {}\nNEW ISSUE\nTitle: {}\nContent:\n{}",
                    hook.repository.full_name,
                    issue.title,
                    truncate_webhook_content(&body.text, 60, body.has_omitted_images)
                ),
                image_urls: body.image_urls,
            }))
        }
        "issue_comment" if hook.action.as_deref() != Some("created") => Ok(None),
        "issue_comment" => {
            let issue = hook
                .issue
                .as_ref()
                .context("issue_comment payload is missing issue")?;
            let login = hook
                .comment
                .as_ref()
                .and_then(|comment| comment.user.as_ref())
                .map(|user| user.login.as_str())
                .unwrap_or("unknown user");
            let body = parse_markdown(
                hook.comment
                    .as_ref()
                    .and_then(|comment| comment.body.as_deref())
                    .unwrap_or_default(),
            )?;
            Ok(Some(PrintContent {
                text: format!(
                    "{now}\nREPO: {}\nISSUE: {}\nComment by {login}\nContent:\n{}",
                    hook.repository.full_name,
                    issue.title,
                    truncate_webhook_content(&body.text, 60, body.has_omitted_images)
                ),
                image_urls: body.image_urls,
            }))
        }
        "ping" => Ok(Some(PrintContent {
            text: format!(
                "{now}\nREPO: {}\n{}\nSETUP COMPLETE",
                hook.repository.full_name,
                hook.zen.as_deref().unwrap_or_default()
            ),
            image_urls: Vec::new(),
        })),
        _ => {
            let event = hook
                .action
                .as_ref()
                .map_or(event.to_owned(), |action| format!("{event} ({action})"));
            Ok(Some(PrintContent {
                text: format!(
                    "{now}\nREPO: {}\nEvent: {}\n",
                    hook.repository.full_name, event
                ),
                image_urls: Vec::new(),
            }))
        }
    }
}

fn truncate(value: &str, max_chars: usize) -> &str {
    match value.char_indices().nth(max_chars) {
        Some((byte_index, _)) => &value[..byte_index],
        None => value,
    }
}

fn truncate_webhook_content(value: &str, max_chars: usize, has_omitted_images: bool) -> String {
    let value = value.trim();
    let truncated = truncate(value, max_chars);
    let mut result = truncated.to_owned();
    if has_omitted_images && value.chars().count() > max_chars {
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str("[image omitted]");
    }
    result
}

#[derive(Deserialize)]
struct GithubWebhook {
    zen: Option<String>,
    action: Option<String>,
    issue: Option<Issue>,
    comment: Option<Comment>,
    repository: Repository,
}

#[derive(Deserialize)]
struct Repository {
    full_name: String,
}

#[derive(Deserialize)]
struct Comment {
    body: Option<String>,
    user: Option<User>,
}

#[derive(Deserialize)]
struct User {
    login: String,
}

#[derive(Deserialize)]
struct Issue {
    title: String,
    body: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{Method, Request, request};
    use std::collections::VecDeque as TestVecDeque;
    use std::sync::mpsc::{Receiver as EventReceiver, Sender as EventSender};
    use std::thread;
    use tower::ServiceExt;

    #[derive(Debug, Eq, PartialEq)]
    enum TransportEventKind {
        Connect,
        Write(Vec<u8>),
        Read(Vec<u8>),
        ReadFailure,
        Disconnect,
    }

    #[derive(Debug)]
    struct TimedTransportEvent {
        at: Instant,
        kind: TransportEventKind,
    }

    enum MockRead {
        Data(Vec<u8>),
        Failure(&'static str),
    }

    struct WorkerMockTransport {
        connected: bool,
        reads: TestVecDeque<MockRead>,
        events: EventSender<TimedTransportEvent>,
    }

    impl WorkerMockTransport {
        fn new(
            reads: impl IntoIterator<Item = MockRead>,
        ) -> (Self, EventReceiver<TimedTransportEvent>) {
            let (events, event_receiver) = mpsc::channel();
            (
                Self {
                    connected: false,
                    reads: reads.into_iter().collect(),
                    events,
                },
                event_receiver,
            )
        }

        fn record(&self, kind: TransportEventKind) {
            self.events
                .send(TimedTransportEvent {
                    at: Instant::now(),
                    kind,
                })
                .expect("worker transport event receiver must remain open");
        }
    }

    impl Transport for WorkerMockTransport {
        fn connect(&mut self) -> Result<()> {
            self.record(TransportEventKind::Connect);
            self.connected = true;
            Ok(())
        }

        fn is_connected(&self) -> bool {
            self.connected
        }

        fn write_all(&mut self, data: &[u8]) -> Result<()> {
            if !self.connected {
                return Err(anyhow!("mock transport is disconnected"));
            }
            self.record(TransportEventKind::Write(data.to_vec()));
            Ok(())
        }

        fn read(&mut self, _timeout: Duration) -> Result<Vec<u8>> {
            match self.reads.pop_front() {
                Some(MockRead::Data(data)) => {
                    self.record(TransportEventKind::Read(data.clone()));
                    Ok(data)
                }
                Some(MockRead::Failure(message)) => {
                    self.record(TransportEventKind::ReadFailure);
                    Err(anyhow!(message))
                }
                None => {
                    self.record(TransportEventKind::ReadFailure);
                    Err(anyhow!("mock response queue is empty"))
                }
            }
        }

        fn disconnect(&mut self) -> Result<()> {
            self.record(TransportEventKind::Disconnect);
            self.connected = false;
            Ok(())
        }
    }

    fn worker_session_config() -> SessionConfig {
        SessionConfig {
            command_delay: Duration::ZERO,
            ..SessionConfig::default()
        }
    }

    fn initialize_worker_session(
        reads: impl IntoIterator<Item = MockRead>,
    ) -> (
        PrinterSession<WorkerMockTransport>,
        EventReceiver<TimedTransportEvent>,
    ) {
        let (transport, events) = WorkerMockTransport::new(reads);
        let mut session = PrinterSession::new(transport, worker_session_config());
        session.initialize().unwrap();
        while events.try_recv().is_ok() {}
        (session, events)
    }

    fn wait_for_event(
        events: &EventReceiver<TimedTransportEvent>,
        mut predicate: impl FnMut(&TransportEventKind) -> bool,
    ) -> TimedTransportEvent {
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            let event = events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("expected worker transport event within 500 ms");
            if predicate(&event.kind) {
                return event;
            }
        }
    }

    fn worker_test_job(job_id: &str) -> PrintJob {
        PrintJob {
            job_id: job_id.to_owned(),
            text: "Worker test".to_owned(),
            image_urls: Vec::new(),
        }
    }

    fn test_state(capacity: usize) -> (Arc<AppState>, Receiver<PrintJob>) {
        let (queue, receiver) = mpsc::sync_channel(capacity);
        (
            Arc::new(AppState {
                queue: Mutex::new(Some(queue)),
                dedupe: Mutex::new(DedupeCache::new(DEDUPE_CACHE_CAPACITY)),
            }),
            receiver,
        )
    }

    fn post_request(path: &str) -> request::Builder {
        Request::builder().method(Method::POST).uri(path)
    }

    async fn send(app: &Router, request: Request<Body>) -> Response {
        app.clone().oneshot(request).await.unwrap()
    }

    fn issue_hook(action: &str) -> GithubWebhook {
        GithubWebhook {
            zen: None,
            action: Some(action.to_owned()),
            issue: Some(Issue {
                title: "Printer test".to_owned(),
                body: Some("Body [link](https://example.com) tail".to_owned()),
            }),
            comment: None,
            repository: Repository {
                full_name: "owner/repository".to_owned(),
            },
        }
    }

    #[test]
    fn print_worker_resets_health_deadline_after_print_attempt() {
        let health_check_interval = Duration::from_millis(25);
        let (session, events) = initialize_worker_session([
            MockRead::Data(b"OK".to_vec()),
            MockRead::Data(b"OK".to_vec()),
            MockRead::Data(vec![0]),
            MockRead::Data(vec![0xAA]),
            MockRead::Data(vec![0]),
        ]);
        let (queue, receiver) = mpsc::sync_channel(1);
        queue.try_send(worker_test_job("deadline-reset")).unwrap();
        let worker = thread::spawn(move || {
            run_print_worker(receiver, session, health_check_interval);
        });

        let print_completed = wait_for_event(&events, |event| {
            *event == TransportEventKind::Read(vec![0xAA])
        });
        let health_query = wait_for_event(&events, |event| {
            *event
                == TransportEventKind::Write(
                    rs_luck_jingle::protocol::compile(
                        rs_luck_jingle::protocol::Command::QueryStatus,
                    )
                    .unwrap()
                    .bytes,
                )
        });
        wait_for_event(&events, |event| *event == TransportEventKind::Read(vec![0]));

        assert!(health_query.at.duration_since(print_completed.at) >= health_check_interval);

        drop(queue);
        worker.join().unwrap();
        wait_for_event(&events, |event| *event == TransportEventKind::Disconnect);
    }

    #[test]
    fn print_worker_retries_health_check_and_reconnects_on_next_deadline() {
        let health_check_interval = Duration::from_millis(20);
        let (session, events) = initialize_worker_session([
            MockRead::Data(b"OK".to_vec()),
            MockRead::Data(b"OK".to_vec()),
            MockRead::Failure("mock health timeout"),
            MockRead::Data(b"OK".to_vec()),
            MockRead::Data(b"OK".to_vec()),
            MockRead::Data(vec![0]),
        ]);
        let (queue, receiver) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            run_print_worker(receiver, session, health_check_interval);
        });

        let failed_health_check =
            wait_for_event(&events, |event| *event == TransportEventKind::ReadFailure);
        wait_for_event(&events, |event| *event == TransportEventKind::Disconnect);
        let reconnect = wait_for_event(&events, |event| *event == TransportEventKind::Connect);
        wait_for_event(&events, |event| *event == TransportEventKind::Read(vec![0]));

        assert!(reconnect.at.duration_since(failed_health_check.at) >= health_check_interval);

        drop(queue);
        worker.join().unwrap();
        wait_for_event(&events, |event| *event == TransportEventKind::Disconnect);
    }

    #[test]
    fn print_worker_coalesces_an_overdue_health_check_before_a_queued_job() {
        let (session, events) = initialize_worker_session([
            MockRead::Data(b"OK".to_vec()),
            MockRead::Data(b"OK".to_vec()),
            MockRead::Data(vec![0]),
            MockRead::Data(vec![0]),
            MockRead::Data(vec![0xAA]),
        ]);
        let (queue, receiver) = mpsc::sync_channel(1);
        queue.try_send(worker_test_job("overdue-check")).unwrap();
        drop(queue);

        let worker = thread::spawn(move || {
            run_print_worker(receiver, session, Duration::ZERO);
        });
        worker.join().unwrap();

        let event_kinds: Vec<_> = events.try_iter().map(|event| event.kind).collect();
        let status_query =
            rs_luck_jingle::protocol::compile(rs_luck_jingle::protocol::Command::QueryStatus)
                .unwrap()
                .bytes;
        assert_eq!(
            event_kinds
                .iter()
                .filter(|event| { **event == TransportEventKind::Write(status_query.clone()) })
                .count(),
            2
        );
        assert_eq!(
            event_kinds
                .iter()
                .filter(|event| **event == TransportEventKind::Disconnect)
                .count(),
            1
        );
        let first_status_query = event_kinds
            .iter()
            .position(|event| *event == TransportEventKind::Write(status_query.clone()))
            .unwrap();
        let enable_printing = event_kinds
            .iter()
            .position(|event| {
                *event
                    == TransportEventKind::Write(
                        rs_luck_jingle::protocol::compile(
                            rs_luck_jingle::protocol::Command::EnablePrinter,
                        )
                        .unwrap()
                        .bytes,
                    )
            })
            .unwrap();
        assert!(first_status_query < enable_printing);
    }

    #[test]
    fn http_listener_conflict_is_detected_before_startup_work() {
        let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = occupied.local_addr().unwrap();

        let error = bind_http_listener(&address.to_string()).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
    }

    #[tokio::test]
    async fn print_markdown_enqueues_text_and_images() {
        let (state, receiver) = test_state(4);
        let app = build_router(state);
        let image_url = "https://github.com/user-attachments/assets/test-image";
        let request = post_request("/print")
            .header(header::CONTENT_TYPE, "text/markdown; charset=UTF-8")
            .body(Body::from(format!(
                "# Receipt\nBody [link](https://example.com)\n![scan]({image_url})"
            )))
            .unwrap();

        let response = send(&app, request).await;

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let request_id = response
            .headers()
            .get(REQUEST_ID_HEADER)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(request_id.starts_with("job-"));
        assert!(request_id.bytes().all(|byte| byte.is_ascii_graphic()));
        let job = receiver.try_recv().unwrap();
        assert_eq!(job.job_id, request_id);
        assert_eq!(job.text, "Receipt\nBody link");
        assert_eq!(job.image_urls, [image_url]);
    }

    #[tokio::test]
    async fn print_markdown_accepts_image_only_content() {
        let (state, receiver) = test_state(1);
        let app = build_router(state);
        let image_url = "https://github.com/user-attachments/assets/image-only";
        let request = post_request("/print")
            .header(header::CONTENT_TYPE, "text/markdown")
            .body(Body::from(format!("![scan]({image_url})")))
            .unwrap();

        let response = send(&app, request).await;

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let job = receiver.try_recv().unwrap();
        assert!(job.text.is_empty());
        assert_eq!(job.image_urls, [image_url]);
    }

    #[tokio::test]
    async fn print_markdown_rejects_missing_or_wrong_content_type() {
        let (state, receiver) = test_state(4);
        let app = build_router(state);
        let requests = [
            post_request("/print").body(Body::from("body")).unwrap(),
            post_request("/print")
                .header(header::CONTENT_TYPE, "text/plain")
                .body(Body::from("body"))
                .unwrap(),
            post_request("/print")
                .header(header::CONTENT_TYPE, "text/markdown; charset=iso-8859-1")
                .body(Body::from("body"))
                .unwrap(),
            post_request("/print")
                .header(header::CONTENT_TYPE, "text/markdown; profile=receipt")
                .body(Body::from("body"))
                .unwrap(),
            post_request("/print")
                .header(header::CONTENT_TYPE, "text/markdown")
                .header(header::CONTENT_TYPE, "text/markdown")
                .body(Body::from("body"))
                .unwrap(),
        ];

        for request in requests {
            let response = send(&app, request).await;
            assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        }
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn print_markdown_rejects_invalid_utf8_and_empty_content() {
        let (state, receiver) = test_state(2);
        let app = build_router(state);
        let invalid_utf8 = post_request("/print")
            .header(header::CONTENT_TYPE, "text/markdown; charset=utf-8")
            .body(Body::from(vec![0xff]))
            .unwrap();
        let empty = post_request("/print")
            .header(header::CONTENT_TYPE, "text/markdown")
            .body(Body::from("  \n\t"))
            .unwrap();

        let invalid_utf8_response = send(&app, invalid_utf8).await;
        let empty_response = send(&app, empty).await;

        assert_eq!(invalid_utf8_response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(empty_response.status(), StatusCode::BAD_REQUEST);
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn print_markdown_enforces_the_exact_body_limit() {
        let (state, receiver) = test_state(1);
        let app = build_router(state);
        let at_limit = post_request("/print")
            .header(header::CONTENT_TYPE, "text/markdown")
            .body(Body::from(vec![b'a'; MAX_MARKDOWN_BODY_BYTES]))
            .unwrap();
        let over_limit = post_request("/print")
            .header(header::CONTENT_TYPE, "text/markdown")
            .body(Body::from(vec![b'a'; MAX_MARKDOWN_BODY_BYTES + 1]))
            .unwrap();

        let accepted = send(&app, at_limit).await;
        let rejected = send(&app, over_limit).await;

        assert_eq!(accepted.status(), StatusCode::ACCEPTED);
        assert_eq!(rejected.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            receiver.try_recv().unwrap().text.len(),
            MAX_MARKDOWN_BODY_BYTES
        );
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn print_markdown_rejects_invalid_idempotency_keys() {
        let (state, receiver) = test_state(2);
        let app = build_router(state);
        let invalid_keys = ["bad key".to_owned(), "x".repeat(129)];

        for key in invalid_keys {
            let request = post_request("/print")
                .header(header::CONTENT_TYPE, "text/markdown")
                .header(IDEMPOTENCY_KEY_HEADER, key)
                .body(Body::from("body"))
                .unwrap();
            let response = send(&app, request).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }

        let duplicate_key = post_request("/print")
            .header(header::CONTENT_TYPE, "text/markdown")
            .header(IDEMPOTENCY_KEY_HEADER, "first")
            .header(IDEMPOTENCY_KEY_HEADER, "second")
            .body(Body::from("body"))
            .unwrap();
        let response = send(&app, duplicate_key).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn print_markdown_returns_retry_after_and_does_not_cache_queue_failures() {
        let (queue, receiver) = mpsc::sync_channel(1);
        queue
            .try_send(PrintJob {
                job_id: "occupied".to_owned(),
                text: "occupied".to_owned(),
                image_urls: Vec::new(),
            })
            .unwrap();
        let state = Arc::new(AppState {
            queue: Mutex::new(Some(queue)),
            dedupe: Mutex::new(DedupeCache::new(DEDUPE_CACHE_CAPACITY)),
        });
        let app = build_router(state.clone());
        let make_request = || {
            post_request("/print")
                .header(header::CONTENT_TYPE, "text/markdown")
                .header(IDEMPOTENCY_KEY_HEADER, "retry-key")
                .body(Body::from("retry me"))
                .unwrap()
        };

        let failed = send(&app, make_request()).await;

        assert_eq!(failed.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(failed.headers().get(header::RETRY_AFTER).unwrap(), "1");
        assert!(
            state
                .dedupe
                .lock()
                .unwrap()
                .get(&DedupeKey::PrintIdempotency("retry-key".to_owned()))
                .is_none()
        );

        assert_eq!(receiver.try_recv().unwrap().job_id, "occupied");
        let retried = send(&app, make_request()).await;
        assert_eq!(retried.status(), StatusCode::ACCEPTED);
        assert_eq!(receiver.try_recv().unwrap().text, "retry me");
    }

    #[tokio::test]
    async fn print_markdown_returns_retry_after_when_queue_is_closed() {
        let (state, receiver) = test_state(1);
        drop(receiver);
        let app = build_router(state);
        let request = post_request("/print")
            .header(header::CONTENT_TYPE, "text/markdown")
            .body(Body::from("body"))
            .unwrap();

        let response = send(&app, request).await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get(header::RETRY_AFTER).unwrap(), "1");
    }

    #[tokio::test]
    async fn closing_queue_disconnects_worker_while_router_holds_state() {
        let (state, receiver) = test_state(1);
        let app = build_router(state.clone());
        state.dedupe.lock().unwrap().insert(
            DedupeKey::PrintIdempotency("closed-key".to_owned()),
            "existing-job".to_owned(),
        );
        state.close_queue();

        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));

        let request = post_request("/print")
            .header(header::CONTENT_TYPE, "text/markdown")
            .header(IDEMPOTENCY_KEY_HEADER, "closed-key")
            .body(Body::from("body"))
            .unwrap();
        let response = send(&app, request).await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get(header::RETRY_AFTER).unwrap(), "1");
    }

    #[tokio::test]
    async fn print_markdown_deduplicates_valid_idempotency_keys() {
        let (state, receiver) = test_state(2);
        let app = build_router(state);
        let make_request = || {
            post_request("/print")
                .header(header::CONTENT_TYPE, "text/markdown")
                .header(IDEMPOTENCY_KEY_HEADER, "stable-key")
                .body(Body::from("body"))
                .unwrap()
        };

        let first = send(&app, make_request()).await;
        let second = send(&app, make_request()).await;

        assert_eq!(first.status(), StatusCode::ACCEPTED);
        assert_eq!(second.status(), StatusCode::ACCEPTED);
        assert_eq!(
            first.headers().get(REQUEST_ID_HEADER),
            second.headers().get(REQUEST_ID_HEADER)
        );
        let job = receiver.try_recv().unwrap();
        assert_eq!(
            first.headers().get(REQUEST_ID_HEADER).unwrap(),
            job.job_id.as_str()
        );
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn print_markdown_without_idempotency_key_always_enqueues() {
        let (state, receiver) = test_state(2);
        let app = build_router(state);
        let make_request = || {
            post_request("/print")
                .header(header::CONTENT_TYPE, "text/markdown")
                .body(Body::from("body"))
                .unwrap()
        };

        let first = send(&app, make_request()).await;
        let second = send(&app, make_request()).await;

        assert_eq!(first.status(), StatusCode::ACCEPTED);
        assert_eq!(second.status(), StatusCode::ACCEPTED);
        assert_ne!(
            first.headers().get(REQUEST_ID_HEADER),
            second.headers().get(REQUEST_ID_HEADER)
        );
        assert_ne!(
            receiver.try_recv().unwrap().job_id,
            receiver.try_recv().unwrap().job_id
        );
    }

    #[tokio::test]
    async fn print_and_github_idempotency_namespaces_do_not_collide() {
        let (state, receiver) = test_state(2);
        let app = build_router(state);
        let print_request = post_request("/print")
            .header(header::CONTENT_TYPE, "text/markdown")
            .header(IDEMPOTENCY_KEY_HEADER, "shared-key")
            .body(Body::from("body"))
            .unwrap();
        let github_request = post_request("/github-webhooks")
            .header(header::CONTENT_TYPE, "application/json")
            .header("X-GitHub-Event", "ping")
            .header("X-GitHub-Delivery", "shared-key")
            .body(Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "zen": "Keep it logically awesome.",
                    "repository": { "full_name": "owner/repository" }
                }))
                .unwrap(),
            ))
            .unwrap();

        let print_response = send(&app, print_request).await;
        let github_response = send(&app, github_request).await;

        assert_eq!(print_response.status(), StatusCode::ACCEPTED);
        assert_eq!(github_response.status(), StatusCode::ACCEPTED);
        assert_ne!(
            print_response.headers().get(REQUEST_ID_HEADER),
            github_response.headers().get(REQUEST_ID_HEADER)
        );
        assert_ne!(
            receiver.try_recv().unwrap().job_id,
            receiver.try_recv().unwrap().job_id
        );
    }

    #[tokio::test]
    async fn github_webhook_preserves_json_rejection_status_codes() {
        let (state, receiver) = test_state(1);
        let app = build_router(state);
        let missing_content_type = post_request("/github-webhooks")
            .header("X-GitHub-Event", "ping")
            .body(Body::from(
                r#"{"repository":{"full_name":"owner/repository"}}"#,
            ))
            .unwrap();
        let invalid_data = post_request("/github-webhooks")
            .header(header::CONTENT_TYPE, "application/json")
            .header("X-GitHub-Event", "ping")
            .body(Body::from("{}"))
            .unwrap();
        let oversized = post_request("/github-webhooks")
            .header(header::CONTENT_TYPE, "application/json")
            .header("X-GitHub-Event", "ping")
            .body(Body::from(vec![b' '; 2 * 1024 * 1024 + 1]))
            .unwrap();

        assert_eq!(
            send(&app, missing_content_type).await.status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            send(&app, invalid_data).await.status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            send(&app, oversized).await.status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn router_preserves_method_and_path_behavior() {
        let (state, _receiver) = test_state(1);
        let app = build_router(state);
        let index = Request::builder()
            .method(Method::GET)
            .uri("/")
            .body(Body::empty())
            .unwrap();
        let head = Request::builder()
            .method(Method::HEAD)
            .uri("/")
            .body(Body::empty())
            .unwrap();
        let wrong_method = Request::builder()
            .method(Method::GET)
            .uri("/print")
            .body(Body::empty())
            .unwrap();
        let wrong_webhook_method = Request::builder()
            .method(Method::GET)
            .uri("/github-webhooks")
            .body(Body::empty())
            .unwrap();
        let trailing_slash = post_request("/print/")
            .header(header::CONTENT_TYPE, "text/markdown")
            .body(Body::from("body"))
            .unwrap();

        let index = send(&app, index).await;
        let index_status = index.status();
        let index_body = axum::body::to_bytes(index.into_body(), usize::MAX)
            .await
            .unwrap();

        assert_eq!(index_status, StatusCode::OK);
        assert_eq!(index_body, "Ok");
        let head = send(&app, head).await;
        assert_eq!(head.status(), StatusCode::NOT_FOUND);
        assert!(head.headers().get(header::ALLOW).is_none());
        let wrong_method = send(&app, wrong_method).await;
        assert_eq!(wrong_method.status(), StatusCode::NOT_FOUND);
        assert!(wrong_method.headers().get(header::ALLOW).is_none());
        let wrong_webhook_method = send(&app, wrong_webhook_method).await;
        assert_eq!(wrong_webhook_method.status(), StatusCode::NOT_FOUND);
        assert!(wrong_webhook_method.headers().get(header::ALLOW).is_none());
        assert_eq!(
            send(&app, trailing_slash).await.status(),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn ignores_non_opened_issue_events() {
        assert!(
            build_message("issues", &issue_hook("closed"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn builds_issue_message_with_link_labels() {
        let message = build_message("issues", &issue_hook("opened"))
            .unwrap()
            .unwrap();

        assert!(message.text.contains("owner/repository"));
        assert!(message.text.contains("Printer test"));
        assert!(message.text.contains("Body link tail"));
        assert!(!message.text.contains("https://"));
        assert!(message.image_urls.is_empty());
    }

    #[test]
    fn preserves_issue_images_after_text_truncation() {
        let image_url = "https://github.com/user-attachments/assets/example-image";
        let mut hook = issue_hook("opened");
        hook.issue.as_mut().unwrap().body = Some(format!(
            "{}\n![receipt]({image_url})",
            "Long issue content ".repeat(20)
        ));

        let message = build_message("issues", &hook).unwrap().unwrap();

        assert_eq!(message.image_urls, [image_url]);
        let content = message.text.split("Content:\n").nth(1).unwrap();
        assert_eq!(content.chars().count(), 60);
        assert!(!message.text.contains("receipt"));
    }

    #[test]
    fn builds_comment_message_with_body_and_image() {
        let image_url = "https://user-images.githubusercontent.com/example/image.png";
        let mut hook = issue_hook("opened");
        hook.action = Some("created".to_owned());
        hook.comment = Some(Comment {
            body: Some(format!("Looks good\n![preview]({image_url})")),
            user: Some(User {
                login: "octocat".to_owned(),
            }),
        });

        let message = build_message("issue_comment", &hook).unwrap().unwrap();

        assert!(message.text.contains("Comment by octocat"));
        assert!(message.text.contains("Looks good"));
        assert_eq!(message.image_urls, [image_url]);
    }

    #[test]
    fn preserves_external_https_images_for_download() {
        let mut hook = issue_hook("opened");
        hook.issue.as_mut().unwrap().body =
            Some("Before ![remote](https://example.test/image.png) after".to_owned());

        let message = build_message("issues", &hook).unwrap().unwrap();

        assert!(message.text.contains("Before  after"));
        assert_eq!(message.image_urls, ["https://example.test/image.png"]);
    }

    #[test]
    fn failed_image_download_adds_a_printable_fallback() {
        let job = PrintJob {
            job_id: "test-job".to_owned(),
            text: "Webhook text".to_owned(),
            image_urls: vec!["https://127.0.0.1/image.png".to_owned()],
        };

        let text = render_text(&job.text).unwrap();
        let rendered = render_print_job(&job).unwrap();

        assert_eq!(rendered.width(), rs_luck_jingle::protocol::PRINT_WIDTH_DOTS);
        assert!(rendered.height() > text.height());
    }

    #[test]
    fn image_only_job_does_not_add_an_empty_text_section() {
        let job = PrintJob {
            job_id: "image-only-job".to_owned(),
            text: String::new(),
            image_urls: vec!["https://127.0.0.1/image.png".to_owned()],
        };

        let fallback = render_text("[image unavailable]").unwrap();
        let rendered = render_print_job(&job).unwrap();

        assert_eq!(rendered.dimensions(), fallback.dimensions());
    }

    #[test]
    fn dedupe_cache_namespaces_keys_and_evicts_oldest() {
        let mut cache = DedupeCache::new(2);
        let print_one = DedupeKey::PrintIdempotency("one".to_owned());
        let delivery_one = DedupeKey::GithubDelivery("one".to_owned());
        let print_two = DedupeKey::PrintIdempotency("two".to_owned());

        cache.insert(print_one.clone(), "job-1".to_owned());
        cache.insert(delivery_one.clone(), "job-2".to_owned());
        assert_eq!(cache.get(&print_one), Some("job-1"));
        assert_eq!(cache.get(&delivery_one), Some("job-2"));

        cache.insert(print_two.clone(), "job-3".to_owned());
        assert_eq!(cache.get(&print_one), None);
        assert_eq!(cache.get(&delivery_one), Some("job-2"));
        assert_eq!(cache.get(&print_two), Some("job-3"));
    }

    #[test]
    fn truncate_preserves_utf8_boundaries() {
        assert_eq!(truncate("abcdef", 3), "abc");
        assert_eq!(truncate("one two", 20), "one two");
    }

    #[test]
    fn headless_multiple_printers_list_addresses_without_reading_input() {
        let candidates = [
            PrinterCandidate {
                name: "LuckP_D1X_A".to_owned(),
                address: "02:00:00:00:00:01".to_owned(),
                rssi: Some(-40),
            },
            PrinterCandidate {
                name: "LuckP_D1X_B".to_owned(),
                address: "02:00:00:00:00:02".to_owned(),
                rssi: Some(-50),
            },
        ];
        let mut input = io::Cursor::new(b"1\n".as_slice());
        let mut output = Vec::new();

        let error =
            select_discovered_printer(&candidates, &mut input, &mut output, false).unwrap_err();

        assert!(error.to_string().contains("LUCK_JINGLE_PRINTER_ADDRESS"));
        assert_eq!(input.position(), 0);
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("02:00:00:00:00:01"));
        assert!(output.contains("02:00:00:00:00:02"));
    }
}
