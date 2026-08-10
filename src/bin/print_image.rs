use std::env;
use std::ffi::OsString;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use rs_luck_jingle::discovery::{
    PrinterCandidate, discover_printers, select_printer, write_printer_candidates,
};
use rs_luck_jingle::protocol::{DEFAULT_FEED_DOTS, Density};
use rs_luck_jingle::render::load_image;
use rs_luck_jingle::session::{PrinterSession, SessionConfig};
use rs_luck_jingle::transport::RfcommTransport;

const DEFAULT_IMAGE_PATH: &str = "res/test_image.png";
const DEFAULT_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(12);

struct RuntimeConfig {
    printer_address: Option<String>,
    rfcomm_channel: Option<u8>,
    discovery_timeout: Duration,
    session: SessionConfig,
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
            bail!("LUCK_JINGLE_DISCOVERY_TIMEOUT_SECS must be greater than zero");
        }

        let density_level = parse_env("LUCK_JINGLE_DENSITY", u8::from(Density::Normal))?;
        let density = Density::try_from(density_level)?;
        let feed_dots = parse_env("LUCK_JINGLE_FEED_DOTS", DEFAULT_FEED_DOTS)?;

        Ok(Self {
            printer_address,
            rfcomm_channel,
            discovery_timeout: Duration::from_secs(discovery_timeout_secs),
            session: SessionConfig {
                density,
                feed_dots,
                ..SessionConfig::default()
            },
        })
    }
}

fn main() -> Result<()> {
    #[cfg(target_os = "macos")]
    if let Some(result) = rs_luck_jingle::macos_bluetooth::run_helper_if_requested() {
        return result;
    }

    env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to create the Tokio runtime")?;
    runtime.block_on(run())
}

async fn run() -> Result<()> {
    let image_path = PathBuf::from(
        env::args_os()
            .nth(1)
            .unwrap_or_else(|| OsString::from(DEFAULT_IMAGE_PATH)),
    );
    let config = RuntimeConfig::from_env()?;
    let image = load_image(&image_path)
        .with_context(|| format!("failed to load image {}", image_path.display()))?;
    let dimensions = image.dimensions();

    let printer_address =
        resolve_printer_address(config.printer_address, config.discovery_timeout).await?;
    let transport = build_transport(&printer_address, config.rfcomm_channel).await?;
    let session_config = config.session;
    let outcome = tokio::task::spawn_blocking(move || {
        let mut session = PrinterSession::new(transport, session_config);
        session
            .print(&image)
            .map_err(|failure| failure.into_error())
            .context("failed to print image")
    })
    .await
    .context("image print task failed")??;

    println!(
        "Printed image: path={}, dimensions={}x{}, raster_bytes={}",
        image_path.display(),
        dimensions.0,
        dimensions.1,
        outcome.raster_bytes
    );
    Ok(())
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

async fn resolve_printer_address(
    configured_address: Option<String>,
    discovery_timeout: Duration,
) -> Result<String> {
    if let Some(address) = configured_address {
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
            "multiple printers were discovered without an interactive terminal; set \
             LUCK_JINGLE_PRINTER_ADDRESS to one of the listed MAC addresses"
        ));
    }

    select_printer(candidates, input, output)
}

async fn build_transport(address: &str, channel: Option<u8>) -> Result<RfcommTransport> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        if let Some(channel) = channel {
            return RfcommTransport::new(address, channel)
                .context("failed to configure the explicit RFCOMM channel");
        }

        RfcommTransport::from_profile(address)
            .await
            .context("failed to connect to the selected printer through the SPP profile")
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (address, channel);
        Err(anyhow!(
            "Bluetooth Classic RFCOMM printing is unsupported on {}",
            env::consts::OS
        ))
    }
}
