use actix_web::http::header::{self, HeaderMap};
use actix_web::{App, HttpRequest, HttpResponse, HttpServer, Responder, get, post, web};
use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use rs_luck_jingle::discovery::{
    PrinterCandidate, discover_printers, select_printer, write_printer_candidates,
};
use rs_luck_jingle::markdown::{MarkdownImageFetcher, parse_markdown};
use rs_luck_jingle::protocol::Density;
use rs_luck_jingle::render::{load_image_bytes, render_text, stack_vertical};
use rs_luck_jingle::session::{PrintFailure, PrinterSession, SessionConfig};
use rs_luck_jingle::transport::RfcommTransport;
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::env;
use std::io::{self, BufRead, IsTerminal, Write};
use std::str::FromStr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc::{self, Receiver, Sender};

const QUEUE_CAPACITY: usize = 100;
const DEDUPE_CACHE_CAPACITY: usize = 1_024;
const MAX_MARKDOWN_BODY_BYTES: usize = 64 * 1024;
const DEFAULT_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(12);
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
    queue: Sender<PrintJob>,
    dedupe: Mutex<DedupeCache>,
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

    actix_web::rt::System::new().block_on(run())
}

async fn run() -> io::Result<()> {
    env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));
    let config = RuntimeConfig::from_env().map_err(io::Error::other)?;
    let printer_address =
        resolve_printer_address(config.printer_address.clone(), config.discovery_timeout)
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
    let (queue, receiver) = mpsc::channel(QUEUE_CAPACITY);
    tokio::task::spawn_blocking(move || run_print_worker(receiver, session));

    let state = web::Data::new(AppState {
        queue,
        dedupe: Mutex::new(DedupeCache::new(DEDUPE_CACHE_CAPACITY)),
    });

    HttpServer::new(move || {
        App::new()
            .app_data(web::PayloadConfig::new(MAX_MARKDOWN_BODY_BYTES))
            .app_data(state.clone())
            .service(index)
            .service(print_markdown)
            .service(github_webhooks)
    })
    .bind(&config.bind_address)?
    .run()
    .await
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

fn run_print_worker(
    mut receiver: Receiver<PrintJob>,
    mut session: PrinterSession<RfcommTransport>,
) {
    while let Some(job) = receiver.blocking_recv() {
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

        match session.print(&image) {
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

#[get("/")]
async fn index() -> impl Responder {
    HttpResponse::Ok().body("Ok")
}

#[post("/print")]
async fn print_markdown(
    state: web::Data<AppState>,
    request: HttpRequest,
    body: web::Bytes,
) -> HttpResponse {
    if !has_markdown_content_type(request.headers()) {
        return HttpResponse::UnsupportedMediaType()
            .body("Content-Type must be text/markdown or text/markdown; charset=utf-8");
    }

    let idempotency_key = match parse_idempotency_key(request.headers()) {
        Ok(key) => key,
        Err(message) => return HttpResponse::BadRequest().body(message),
    };
    let markdown = match std::str::from_utf8(&body) {
        Ok(markdown) => markdown,
        Err(_) => return HttpResponse::BadRequest().body("Markdown body must be valid UTF-8"),
    };
    let content = match parse_markdown(markdown) {
        Ok(content) => content,
        Err(error) => {
            log::warn!("invalid Markdown print request: {error:#}");
            return HttpResponse::BadRequest().body("Markdown body is invalid");
        }
    };
    if content.text.is_empty() && content.image_urls.is_empty() {
        return HttpResponse::BadRequest().body("Markdown body has no printable content");
    }

    let job_id = next_job_id();
    let key = idempotency_key.map(DedupeKey::PrintIdempotency);
    let job = PrintJob {
        job_id: job_id.clone(),
        text: content.text,
        image_urls: content.image_urls,
    };
    match enqueue_print_job(state.get_ref(), key, job) {
        Ok(accepted_job_id) => accepted_response(&accepted_job_id),
        Err(error) => {
            log::error!("failed to enqueue Markdown print job: {error}");
            unavailable_response()
        }
    }
}

#[post("/github-webhooks")]
async fn github_webhooks(
    state: web::Data<AppState>,
    hook: web::Json<GithubWebhook>,
    request: HttpRequest,
) -> impl Responder {
    let event = match header_value(request.headers(), "X-GitHub-Event") {
        Some(event) => event,
        None => return HttpResponse::BadRequest().body("missing X-GitHub-Event"),
    };
    let delivery_id = header_value(request.headers(), "X-GitHub-Delivery")
        .unwrap_or("missing-delivery-id")
        .to_owned();
    let content = match build_message(event, &hook) {
        Ok(Some(content)) => content,
        Ok(None) => return HttpResponse::NoContent().finish(),
        Err(error) => return HttpResponse::BadRequest().body(error.to_string()),
    };

    let job_id = next_job_id();
    let job = PrintJob {
        job_id: job_id.clone(),
        text: content.text,
        image_urls: content.image_urls,
    };
    let key = DedupeKey::GithubDelivery(delivery_id);
    match enqueue_print_job(state.get_ref(), Some(key), job) {
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
) -> Result<String, mpsc::error::TrySendError<PrintJob>> {
    let job_id = job.job_id.clone();
    let Some(dedupe_key) = dedupe_key else {
        state.queue.try_send(job)?;
        return Ok(job_id);
    };

    let mut cache = state.dedupe.lock().expect("dedupe cache mutex poisoned");
    if let Some(existing_job_id) = cache.get(&dedupe_key) {
        return Ok(existing_job_id.to_owned());
    }

    state.queue.try_send(job)?;
    cache.insert(dedupe_key, job_id.clone());
    Ok(job_id)
}

fn accepted_response(job_id: &str) -> HttpResponse {
    HttpResponse::Accepted()
        .insert_header((REQUEST_ID_HEADER, job_id))
        .finish()
}

fn unavailable_response() -> HttpResponse {
    HttpResponse::ServiceUnavailable()
        .insert_header((header::RETRY_AFTER, "1"))
        .finish()
}

fn next_job_id() -> String {
    let sequence = NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed);
    format!(
        "job-{}-{}-{sequence}",
        std::process::id(),
        Utc::now().timestamp_micros()
    )
}

fn has_markdown_content_type(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(header::CONTENT_TYPE);
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
    let mut values = headers.get_all(IDEMPOTENCY_KEY_HEADER);
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
            .expect("visible ASCII must be valid UTF-8")
            .to_owned(),
    ))
}

fn header_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

fn build_message(event: &str, hook: &GithubWebhook) -> Result<Option<PrintContent>> {
    let now = Utc::now().format("%Y-%m-%d %H:%M:%S");
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
        unsupported => Err(anyhow!("unsupported GitHub event: {unsupported}")),
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

    fn test_state(capacity: usize) -> (web::Data<AppState>, Receiver<PrintJob>) {
        let (queue, receiver) = mpsc::channel(capacity);
        (
            web::Data::new(AppState {
                queue,
                dedupe: Mutex::new(DedupeCache::new(DEDUPE_CACHE_CAPACITY)),
            }),
            receiver,
        )
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

    #[actix_web::test]
    async fn print_markdown_enqueues_text_and_images() {
        let (state, mut receiver) = test_state(4);
        let app = actix_web::test::init_service(
            App::new()
                .app_data(web::PayloadConfig::new(MAX_MARKDOWN_BODY_BYTES))
                .app_data(state)
                .service(print_markdown),
        )
        .await;
        let image_url = "https://github.com/user-attachments/assets/test-image";
        let request = actix_web::test::TestRequest::post()
            .uri("/print")
            .insert_header((header::CONTENT_TYPE, "text/markdown; charset=UTF-8"))
            .set_payload(format!(
                "# Receipt\nBody [link](https://example.com)\n![scan]({image_url})"
            ))
            .to_request();

        let response = actix_web::test::call_service(&app, request).await;

        assert_eq!(response.status(), actix_web::http::StatusCode::ACCEPTED);
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

    #[actix_web::test]
    async fn print_markdown_accepts_image_only_content() {
        let (state, mut receiver) = test_state(1);
        let app = actix_web::test::init_service(
            App::new()
                .app_data(web::PayloadConfig::new(MAX_MARKDOWN_BODY_BYTES))
                .app_data(state)
                .service(print_markdown),
        )
        .await;
        let image_url = "https://github.com/user-attachments/assets/image-only";
        let request = actix_web::test::TestRequest::post()
            .uri("/print")
            .insert_header((header::CONTENT_TYPE, "text/markdown"))
            .set_payload(format!("![scan]({image_url})"))
            .to_request();

        let response = actix_web::test::call_service(&app, request).await;

        assert_eq!(response.status(), actix_web::http::StatusCode::ACCEPTED);
        let job = receiver.try_recv().unwrap();
        assert!(job.text.is_empty());
        assert_eq!(job.image_urls, [image_url]);
    }

    #[actix_web::test]
    async fn print_markdown_rejects_missing_or_wrong_content_type() {
        let (state, mut receiver) = test_state(4);
        let app = actix_web::test::init_service(
            App::new()
                .app_data(web::PayloadConfig::new(MAX_MARKDOWN_BODY_BYTES))
                .app_data(state)
                .service(print_markdown),
        )
        .await;
        let requests = [
            actix_web::test::TestRequest::post()
                .uri("/print")
                .set_payload("body")
                .to_request(),
            actix_web::test::TestRequest::post()
                .uri("/print")
                .insert_header((header::CONTENT_TYPE, "text/plain"))
                .set_payload("body")
                .to_request(),
            actix_web::test::TestRequest::post()
                .uri("/print")
                .insert_header((header::CONTENT_TYPE, "text/markdown; charset=iso-8859-1"))
                .set_payload("body")
                .to_request(),
            actix_web::test::TestRequest::post()
                .uri("/print")
                .insert_header((header::CONTENT_TYPE, "text/markdown; profile=receipt"))
                .set_payload("body")
                .to_request(),
            actix_web::test::TestRequest::post()
                .uri("/print")
                .insert_header((header::CONTENT_TYPE, "text/markdown"))
                .append_header((header::CONTENT_TYPE, "text/markdown"))
                .set_payload("body")
                .to_request(),
        ];

        for request in requests {
            let response = actix_web::test::call_service(&app, request).await;
            assert_eq!(
                response.status(),
                actix_web::http::StatusCode::UNSUPPORTED_MEDIA_TYPE
            );
        }
        assert!(receiver.try_recv().is_err());
    }

    #[actix_web::test]
    async fn print_markdown_rejects_invalid_utf8_and_empty_content() {
        let (state, mut receiver) = test_state(2);
        let app = actix_web::test::init_service(
            App::new()
                .app_data(web::PayloadConfig::new(MAX_MARKDOWN_BODY_BYTES))
                .app_data(state)
                .service(print_markdown),
        )
        .await;
        let invalid_utf8 = actix_web::test::TestRequest::post()
            .uri("/print")
            .insert_header((header::CONTENT_TYPE, "text/markdown; charset=utf-8"))
            .set_payload(web::Bytes::from_static(&[0xff]))
            .to_request();
        let empty = actix_web::test::TestRequest::post()
            .uri("/print")
            .insert_header((header::CONTENT_TYPE, "text/markdown"))
            .set_payload("  \n\t")
            .to_request();

        let invalid_utf8_response = actix_web::test::call_service(&app, invalid_utf8).await;
        let empty_response = actix_web::test::call_service(&app, empty).await;

        assert_eq!(
            invalid_utf8_response.status(),
            actix_web::http::StatusCode::BAD_REQUEST
        );
        assert_eq!(
            empty_response.status(),
            actix_web::http::StatusCode::BAD_REQUEST
        );
        assert!(receiver.try_recv().is_err());
    }

    #[actix_web::test]
    async fn print_markdown_rejects_oversized_bodies() {
        let (state, mut receiver) = test_state(1);
        let app = actix_web::test::init_service(
            App::new()
                .app_data(web::PayloadConfig::new(MAX_MARKDOWN_BODY_BYTES))
                .app_data(state)
                .service(print_markdown),
        )
        .await;
        let request = actix_web::test::TestRequest::post()
            .uri("/print")
            .insert_header((header::CONTENT_TYPE, "text/markdown"))
            .set_payload(vec![b'a'; MAX_MARKDOWN_BODY_BYTES + 1])
            .to_request();

        let response = actix_web::test::call_service(&app, request).await;

        assert_eq!(
            response.status(),
            actix_web::http::StatusCode::PAYLOAD_TOO_LARGE
        );
        assert!(receiver.try_recv().is_err());
    }

    #[actix_web::test]
    async fn print_markdown_rejects_invalid_idempotency_keys() {
        let (state, mut receiver) = test_state(2);
        let app = actix_web::test::init_service(
            App::new()
                .app_data(web::PayloadConfig::new(MAX_MARKDOWN_BODY_BYTES))
                .app_data(state)
                .service(print_markdown),
        )
        .await;
        let invalid_keys = ["bad key".to_owned(), "x".repeat(129)];

        for key in invalid_keys {
            let request = actix_web::test::TestRequest::post()
                .uri("/print")
                .insert_header((header::CONTENT_TYPE, "text/markdown"))
                .insert_header((IDEMPOTENCY_KEY_HEADER, key))
                .set_payload("body")
                .to_request();
            let response = actix_web::test::call_service(&app, request).await;
            assert_eq!(response.status(), actix_web::http::StatusCode::BAD_REQUEST);
        }

        let duplicate_key = actix_web::test::TestRequest::post()
            .uri("/print")
            .insert_header((header::CONTENT_TYPE, "text/markdown"))
            .insert_header((IDEMPOTENCY_KEY_HEADER, "first"))
            .append_header((IDEMPOTENCY_KEY_HEADER, "second"))
            .set_payload("body")
            .to_request();
        let response = actix_web::test::call_service(&app, duplicate_key).await;
        assert_eq!(response.status(), actix_web::http::StatusCode::BAD_REQUEST);

        assert!(receiver.try_recv().is_err());
    }

    #[actix_web::test]
    async fn print_markdown_returns_retry_after_and_does_not_cache_queue_failures() {
        let (queue, mut receiver) = mpsc::channel(1);
        queue
            .try_send(PrintJob {
                job_id: "occupied".to_owned(),
                text: "occupied".to_owned(),
                image_urls: Vec::new(),
            })
            .unwrap();
        let state = web::Data::new(AppState {
            queue,
            dedupe: Mutex::new(DedupeCache::new(DEDUPE_CACHE_CAPACITY)),
        });
        let app = actix_web::test::init_service(
            App::new()
                .app_data(web::PayloadConfig::new(MAX_MARKDOWN_BODY_BYTES))
                .app_data(state.clone())
                .service(print_markdown),
        )
        .await;
        let make_request = || {
            actix_web::test::TestRequest::post()
                .uri("/print")
                .insert_header((header::CONTENT_TYPE, "text/markdown"))
                .insert_header((IDEMPOTENCY_KEY_HEADER, "retry-key"))
                .set_payload("retry me")
                .to_request()
        };

        let failed = actix_web::test::call_service(&app, make_request()).await;

        assert_eq!(
            failed.status(),
            actix_web::http::StatusCode::SERVICE_UNAVAILABLE
        );
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
        let retried = actix_web::test::call_service(&app, make_request()).await;
        assert_eq!(retried.status(), actix_web::http::StatusCode::ACCEPTED);
        assert_eq!(receiver.try_recv().unwrap().text, "retry me");
    }

    #[actix_web::test]
    async fn print_markdown_returns_retry_after_when_queue_is_closed() {
        let (state, receiver) = test_state(1);
        drop(receiver);
        let app = actix_web::test::init_service(
            App::new()
                .app_data(web::PayloadConfig::new(MAX_MARKDOWN_BODY_BYTES))
                .app_data(state)
                .service(print_markdown),
        )
        .await;
        let request = actix_web::test::TestRequest::post()
            .uri("/print")
            .insert_header((header::CONTENT_TYPE, "text/markdown"))
            .set_payload("body")
            .to_request();

        let response = actix_web::test::call_service(&app, request).await;

        assert_eq!(
            response.status(),
            actix_web::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(response.headers().get(header::RETRY_AFTER).unwrap(), "1");
    }

    #[actix_web::test]
    async fn print_markdown_deduplicates_valid_idempotency_keys() {
        let (state, mut receiver) = test_state(2);
        let app = actix_web::test::init_service(
            App::new()
                .app_data(web::PayloadConfig::new(MAX_MARKDOWN_BODY_BYTES))
                .app_data(state)
                .service(print_markdown),
        )
        .await;
        let make_request = || {
            actix_web::test::TestRequest::post()
                .uri("/print")
                .insert_header((header::CONTENT_TYPE, "text/markdown"))
                .insert_header((IDEMPOTENCY_KEY_HEADER, "stable-key"))
                .set_payload("body")
                .to_request()
        };

        let first = actix_web::test::call_service(&app, make_request()).await;
        let second = actix_web::test::call_service(&app, make_request()).await;

        assert_eq!(first.status(), actix_web::http::StatusCode::ACCEPTED);
        assert_eq!(second.status(), actix_web::http::StatusCode::ACCEPTED);
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

    #[actix_web::test]
    async fn print_markdown_without_idempotency_key_always_enqueues() {
        let (state, mut receiver) = test_state(2);
        let app = actix_web::test::init_service(
            App::new()
                .app_data(web::PayloadConfig::new(MAX_MARKDOWN_BODY_BYTES))
                .app_data(state)
                .service(print_markdown),
        )
        .await;
        let make_request = || {
            actix_web::test::TestRequest::post()
                .uri("/print")
                .insert_header((header::CONTENT_TYPE, "text/markdown"))
                .set_payload("body")
                .to_request()
        };

        let first = actix_web::test::call_service(&app, make_request()).await;
        let second = actix_web::test::call_service(&app, make_request()).await;

        assert_eq!(first.status(), actix_web::http::StatusCode::ACCEPTED);
        assert_eq!(second.status(), actix_web::http::StatusCode::ACCEPTED);
        assert_ne!(
            first.headers().get(REQUEST_ID_HEADER),
            second.headers().get(REQUEST_ID_HEADER)
        );
        assert_ne!(
            receiver.try_recv().unwrap().job_id,
            receiver.try_recv().unwrap().job_id
        );
    }

    #[actix_web::test]
    async fn print_and_github_idempotency_namespaces_do_not_collide() {
        let (state, mut receiver) = test_state(2);
        let app = actix_web::test::init_service(
            App::new()
                .app_data(web::PayloadConfig::new(MAX_MARKDOWN_BODY_BYTES))
                .app_data(state)
                .service(print_markdown)
                .service(github_webhooks),
        )
        .await;
        let print_request = actix_web::test::TestRequest::post()
            .uri("/print")
            .insert_header((header::CONTENT_TYPE, "text/markdown"))
            .insert_header((IDEMPOTENCY_KEY_HEADER, "shared-key"))
            .set_payload("body")
            .to_request();
        let github_request = actix_web::test::TestRequest::post()
            .uri("/github-webhooks")
            .insert_header(("X-GitHub-Event", "ping"))
            .insert_header(("X-GitHub-Delivery", "shared-key"))
            .set_json(serde_json::json!({
                "zen": "Keep it logically awesome.",
                "repository": { "full_name": "owner/repository" }
            }))
            .to_request();

        let print_response = actix_web::test::call_service(&app, print_request).await;
        let github_response = actix_web::test::call_service(&app, github_request).await;

        assert_eq!(
            print_response.status(),
            actix_web::http::StatusCode::ACCEPTED
        );
        assert_eq!(
            github_response.status(),
            actix_web::http::StatusCode::ACCEPTED
        );
        assert_ne!(
            print_response.headers().get(REQUEST_ID_HEADER),
            github_response.headers().get(REQUEST_ID_HEADER)
        );
        assert_ne!(
            receiver.try_recv().unwrap().job_id,
            receiver.try_recv().unwrap().job_id
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
    fn preserves_omitted_image_notice_after_webhook_truncation() {
        let mut hook = issue_hook("opened");
        hook.issue.as_mut().unwrap().body = Some(format!(
            "{} <img src=\"http://127.0.0.1/image.png\" alt=\"unsafe ] detail\">",
            "Long issue content ".repeat(20)
        ));

        let message = build_message("issues", &hook).unwrap().unwrap();
        let content = message.text.split("Content:\n").nth(1).unwrap();

        assert!(content.ends_with("\n[image omitted]"));
        assert_eq!(content.lines().next().unwrap().chars().count(), 60);
        assert!(message.image_urls.is_empty());
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
