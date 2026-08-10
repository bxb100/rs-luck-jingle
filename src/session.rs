use crate::protocol::{
    DEFAULT_AUTO_SHUTDOWN_MINUTES, DEFAULT_FEED_DOTS, Density, PrinterStatus, enable_printer,
    encode_raster, feed_dots, is_ok_response, is_stop_ack, parse_status, query_status,
    set_auto_shutdown, set_density, stop_print_job, wake_printer,
};
use crate::transport::Transport;
use anyhow::{Context, Result, anyhow};
use image::RgbImage;
use std::error::Error;
use std::fmt;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionState {
    Disconnected,
    Connecting,
    Initializing,
    Ready,
    Printing,
    Recovering,
}

#[derive(Clone, Copy, Debug)]
pub struct SessionConfig {
    pub density: Density,
    pub auto_shutdown_minutes: u16,
    pub feed_dots: u8,
    pub command_delay: Duration,
    pub response_timeout: Duration,
    pub stop_timeout: Duration,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            density: Density::Normal,
            auto_shutdown_minutes: DEFAULT_AUTO_SHUTDOWN_MINUTES,
            feed_dots: DEFAULT_FEED_DOTS,
            command_delay: Duration::from_millis(10),
            response_timeout: Duration::from_secs(3),
            stop_timeout: Duration::from_secs(70),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PrintOutcome {
    pub raster_bytes: usize,
    pub status: PrinterStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HealthCheckOutcome {
    pub status: PrinterStatus,
    pub reconnected: bool,
}

#[derive(Debug)]
pub enum PrintFailure {
    RetrySafe(anyhow::Error),
    OutcomeUnknown(anyhow::Error),
}

impl PrintFailure {
    pub const fn is_retry_safe(&self) -> bool {
        matches!(self, Self::RetrySafe(_))
    }

    pub fn error(&self) -> &anyhow::Error {
        match self {
            Self::RetrySafe(error) | Self::OutcomeUnknown(error) => error,
        }
    }

    pub fn into_error(self) -> anyhow::Error {
        match self {
            Self::RetrySafe(error) | Self::OutcomeUnknown(error) => error,
        }
    }
}

impl fmt::Display for PrintFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RetrySafe(error) => write!(formatter, "retry-safe print failure: {error}"),
            Self::OutcomeUnknown(error) => {
                write!(formatter, "print outcome is unknown: {error}")
            }
        }
    }
}

impl Error for PrintFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.error().as_ref())
    }
}

pub struct PrinterSession<T> {
    transport: T,
    config: SessionConfig,
    state: SessionState,
}

impl<T> PrinterSession<T>
where
    T: Transport,
{
    pub fn new(transport: T, config: SessionConfig) -> Self {
        Self {
            transport,
            config,
            state: SessionState::Disconnected,
        }
    }

    pub fn state(&self) -> SessionState {
        self.state
    }

    pub fn initialize(&mut self) -> Result<()> {
        self.ensure_ready().map(|_| ())
    }

    pub fn health_check(&mut self) -> Result<HealthCheckOutcome> {
        let reconnected = self.ensure_ready()?;
        let status = match self.read_status() {
            Ok(status) => status,
            Err(error) => {
                self.close_dirty_session();
                return Err(error);
            }
        };

        Ok(HealthCheckOutcome {
            status,
            reconnected,
        })
    }

    pub fn print(&mut self, image: &RgbImage) -> std::result::Result<PrintOutcome, PrintFailure> {
        self.ensure_ready().map_err(PrintFailure::RetrySafe)?;

        let status = match self.read_status() {
            Ok(status) => status,
            Err(error) => return self.fail_session(PrintFailure::RetrySafe(error)),
        };
        if !status.is_ready() {
            return Err(PrintFailure::RetrySafe(anyhow!(
                "printer is not ready: {status:?}"
            )));
        }

        let raster = encode_raster(image)
            .context("failed to encode printer raster")
            .map_err(PrintFailure::RetrySafe)?;
        self.state = SessionState::Printing;

        let result = self.execute_print(&raster);
        match result {
            Ok(()) => {
                self.state = SessionState::Ready;
                Ok(PrintOutcome {
                    raster_bytes: raster.len(),
                    status,
                })
            }
            Err(failure) => self.fail_session(failure),
        }
    }

    pub fn disconnect(&mut self) -> Result<()> {
        let result = self.transport.disconnect();
        self.state = SessionState::Disconnected;
        result
    }

    fn ensure_ready(&mut self) -> Result<bool> {
        if self.state == SessionState::Ready && self.transport.is_connected() {
            return Ok(false);
        }

        self.state = SessionState::Connecting;
        if let Err(error) = self.transport.connect() {
            self.state = SessionState::Disconnected;
            return Err(error).context("failed to connect RFCOMM transport");
        }

        self.state = SessionState::Initializing;
        let density = set_density(self.config.density);
        if let Err(error) = self.write_command(&density).and_then(|_| {
            self.read_until(self.config.response_timeout, is_ok_response)
                .map(|_| ())
                .context("printer rejected density command")
        }) {
            self.close_dirty_session();
            return Err(error);
        }

        let auto_shutdown = set_auto_shutdown(self.config.auto_shutdown_minutes);
        if let Err(error) = self.write_command(&auto_shutdown).and_then(|_| {
            self.read_until(self.config.response_timeout, is_ok_response)
                .map(|_| ())
                .context("printer rejected auto-shutdown command")
        }) {
            self.close_dirty_session();
            return Err(error);
        }

        self.state = SessionState::Ready;
        Ok(true)
    }

    fn read_status(&mut self) -> Result<PrinterStatus> {
        self.write_command(&query_status())?;
        let response = self
            .transport
            .read(self.config.response_timeout)
            .context("failed while waiting for printer status")?;
        if response.len() != 1 {
            return Err(anyhow!(
                "invalid printer status response length: expected exactly 1 byte, got {}",
                response.len()
            ));
        }
        parse_status(&response).context("invalid printer status response")
    }

    fn execute_print(&mut self, raster: &[u8]) -> std::result::Result<(), PrintFailure> {
        self.write_command(&enable_printer())
            .context("failed to enable printer")
            .map_err(PrintFailure::RetrySafe)?;
        self.write_command(&wake_printer())
            .context("failed to wake printer")
            .map_err(PrintFailure::RetrySafe)?;
        self.write_command(raster)
            .context("failed to write printer raster")
            .map_err(PrintFailure::OutcomeUnknown)?;
        self.write_command(&feed_dots(self.config.feed_dots))
            .context("failed to feed printed output")
            .map_err(PrintFailure::OutcomeUnknown)?;
        self.write_command(&stop_print_job())
            .context("failed to stop print job")
            .map_err(PrintFailure::OutcomeUnknown)?;
        self.read_until(self.config.stop_timeout, is_stop_ack)
            .map(|_| ())
            .context("failed while waiting for print completion")
            .map_err(PrintFailure::OutcomeUnknown)
    }

    fn write_command(&mut self, data: &[u8]) -> Result<()> {
        if !self.config.command_delay.is_zero() {
            thread::sleep(self.config.command_delay);
        }
        self.transport
            .write_all(data)
            .context("failed to write printer command")
    }

    fn read_until<F>(&mut self, timeout: Duration, predicate: F) -> Result<Vec<u8>>
    where
        F: Fn(&[u8]) -> bool,
    {
        let deadline = Instant::now() + timeout;
        let response = self.read_before(deadline)?;
        if predicate(&response) {
            return Ok(response);
        }
        if response != b"O" {
            return Err(unexpected_response(&response));
        }

        let continuation = self.read_before(deadline)?;
        let mut combined = response;
        combined.extend_from_slice(&continuation);
        if predicate(&combined) {
            return Ok(combined);
        }

        Err(unexpected_response(&combined))
    }

    fn read_before(&mut self, deadline: Instant) -> Result<Vec<u8>> {
        let now = Instant::now();
        if now >= deadline {
            return Err(anyhow!("printer response timeout"));
        }
        self.transport.read(deadline - now)
    }

    fn fail_session<R>(&mut self, failure: PrintFailure) -> std::result::Result<R, PrintFailure> {
        self.close_dirty_session();
        Err(failure)
    }

    fn close_dirty_session(&mut self) {
        self.state = SessionState::Recovering;
        if let Err(disconnect_error) = self.transport.disconnect() {
            log::warn!("failed to close dirty printer session: {disconnect_error:#}");
        }
        self.state = SessionState::Disconnected;
    }
}

fn unexpected_response(response: &[u8]) -> anyhow::Error {
    anyhow!("unexpected printer response ({} bytes)", response.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct MockHandle {
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
        disconnects: Arc<Mutex<usize>>,
    }

    struct MockTransport {
        connected: bool,
        responses: VecDeque<Result<Vec<u8>>>,
        write_attempts: usize,
        fail_write_at: Option<usize>,
        handle: MockHandle,
    }

    impl MockTransport {
        fn new(responses: impl IntoIterator<Item = Vec<u8>>) -> (Self, MockHandle) {
            Self::with_results(responses.into_iter().map(Ok))
        }

        fn with_results(
            responses: impl IntoIterator<Item = Result<Vec<u8>>>,
        ) -> (Self, MockHandle) {
            let handle = MockHandle {
                writes: Arc::new(Mutex::new(Vec::new())),
                disconnects: Arc::new(Mutex::new(0)),
            };
            (
                Self {
                    connected: false,
                    responses: responses.into_iter().collect(),
                    write_attempts: 0,
                    fail_write_at: None,
                    handle: handle.clone(),
                },
                handle,
            )
        }

        fn failing_write_at(mut self, write_index: usize) -> Self {
            self.fail_write_at = Some(write_index);
            self
        }
    }

    impl Transport for MockTransport {
        fn connect(&mut self) -> Result<()> {
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
            let write_index = self.write_attempts;
            self.write_attempts += 1;
            if self.fail_write_at == Some(write_index) {
                return Err(anyhow!("mock write failed at index {write_index}"));
            }
            self.handle.writes.lock().unwrap().push(data.to_vec());
            Ok(())
        }

        fn read(&mut self, _timeout: Duration) -> Result<Vec<u8>> {
            self.responses
                .pop_front()
                .unwrap_or_else(|| Err(anyhow!("mock response queue is empty")))
        }

        fn disconnect(&mut self) -> Result<()> {
            self.connected = false;
            *self.handle.disconnects.lock().unwrap() += 1;
            Ok(())
        }
    }

    fn test_config() -> SessionConfig {
        SessionConfig {
            command_delay: Duration::ZERO,
            ..SessionConfig::default()
        }
    }

    #[test]
    fn initializes_connection_before_accepting_print_jobs() {
        let (transport, handle) = MockTransport::new([b"OK".to_vec(), b"OK".to_vec()]);
        let mut session = PrinterSession::new(transport, test_config());

        session.initialize().unwrap();

        assert_eq!(session.state(), SessionState::Ready);
        assert_eq!(
            handle.writes.lock().unwrap().as_slice(),
            [
                set_density(Density::Normal),
                set_auto_shutdown(DEFAULT_AUTO_SHUTDOWN_MINUTES),
            ]
        );
    }

    #[test]
    fn reapplies_initialization_after_reconnect() {
        let (transport, handle) = MockTransport::new([
            b"OK".to_vec(),
            b"OK".to_vec(),
            b"OK".to_vec(),
            b"OK".to_vec(),
        ]);
        let mut session = PrinterSession::new(transport, test_config());

        session.initialize().unwrap();
        session.disconnect().unwrap();
        session.initialize().unwrap();

        assert_eq!(session.state(), SessionState::Ready);
        assert_eq!(
            handle.writes.lock().unwrap().as_slice(),
            [
                set_density(Density::Normal),
                set_auto_shutdown(DEFAULT_AUTO_SHUTDOWN_MINUTES),
                set_density(Density::Normal),
                set_auto_shutdown(DEFAULT_AUTO_SHUTDOWN_MINUTES),
            ]
        );
        assert_eq!(*handle.disconnects.lock().unwrap(), 1);
    }

    #[test]
    fn health_check_only_queries_status_on_ready_connection() {
        let (transport, handle) =
            MockTransport::new([b"OK".to_vec(), b"OK".to_vec(), vec![0], vec![0]]);
        let mut session = PrinterSession::new(transport, test_config());

        let initial = session.health_check().unwrap();
        let idle = session.health_check().unwrap();

        assert_eq!(
            initial,
            HealthCheckOutcome {
                status: PrinterStatus::from_byte(0),
                reconnected: true,
            }
        );
        assert_eq!(
            idle,
            HealthCheckOutcome {
                status: PrinterStatus::from_byte(0),
                reconnected: false,
            }
        );
        assert_eq!(session.state(), SessionState::Ready);
        assert_eq!(
            handle.writes.lock().unwrap().as_slice(),
            [
                set_density(Density::Normal).to_vec(),
                set_auto_shutdown(DEFAULT_AUTO_SHUTDOWN_MINUTES).to_vec(),
                query_status().to_vec(),
                query_status().to_vec(),
            ]
        );
        assert_eq!(*handle.disconnects.lock().unwrap(), 0);
    }

    #[test]
    fn health_check_treats_non_ready_status_as_healthy() {
        let status_byte = 0b0000_0100;
        let (transport, handle) =
            MockTransport::new([b"OK".to_vec(), b"OK".to_vec(), vec![status_byte]]);
        let mut session = PrinterSession::new(transport, test_config());

        let outcome = session.health_check().unwrap();

        assert_eq!(outcome.status, PrinterStatus::from_byte(status_byte));
        assert!(!outcome.status.is_ready());
        assert!(outcome.reconnected);
        assert_eq!(session.state(), SessionState::Ready);
        assert_eq!(*handle.disconnects.lock().unwrap(), 0);
    }

    #[test]
    fn health_check_status_write_failure_disconnects_session() {
        let (transport, handle) = MockTransport::new([b"OK".to_vec(), b"OK".to_vec()]);
        let mut session = PrinterSession::new(transport.failing_write_at(2), test_config());

        let error = session.health_check().unwrap_err();

        assert!(format!("{error:#}").contains("mock write failed at index 2"));
        assert_eq!(session.state(), SessionState::Disconnected);
        assert_eq!(*handle.disconnects.lock().unwrap(), 1);
    }

    #[test]
    fn health_check_invalid_status_disconnects_session() {
        let (transport, handle) =
            MockTransport::new([b"OK".to_vec(), b"OK".to_vec(), vec![0, 0xAA]]);
        let mut session = PrinterSession::new(transport, test_config());

        let error = session.health_check().unwrap_err();

        assert!(error.to_string().contains("expected exactly 1 byte"));
        assert_eq!(session.state(), SessionState::Disconnected);
        assert_eq!(*handle.disconnects.lock().unwrap(), 1);
    }

    #[test]
    fn health_check_reconnects_and_reinitializes_after_status_timeout() {
        let (transport, handle) = MockTransport::with_results([
            Ok(b"OK".to_vec()),
            Ok(b"OK".to_vec()),
            Err(anyhow!("mock status timeout")),
            Ok(b"OK".to_vec()),
            Ok(b"OK".to_vec()),
            Ok(vec![0]),
        ]);
        let mut session = PrinterSession::new(transport, test_config());

        let first_error = session.health_check().unwrap_err();

        assert!(format!("{first_error:#}").contains("mock status timeout"));
        assert_eq!(session.state(), SessionState::Disconnected);
        assert_eq!(*handle.disconnects.lock().unwrap(), 1);

        let recovered = session.health_check().unwrap();

        assert_eq!(
            recovered,
            HealthCheckOutcome {
                status: PrinterStatus::from_byte(0),
                reconnected: true,
            }
        );
        assert_eq!(session.state(), SessionState::Ready);
        assert_eq!(
            handle.writes.lock().unwrap().as_slice(),
            [
                set_density(Density::Normal).to_vec(),
                set_auto_shutdown(DEFAULT_AUTO_SHUTDOWN_MINUTES).to_vec(),
                query_status().to_vec(),
                set_density(Density::Normal).to_vec(),
                set_auto_shutdown(DEFAULT_AUTO_SHUTDOWN_MINUTES).to_vec(),
                query_status().to_vec(),
            ]
        );
        assert_eq!(*handle.disconnects.lock().unwrap(), 1);
    }

    #[test]
    fn reapplies_initialization_after_dirty_session_recovery() {
        let (transport, handle) = MockTransport::new([
            b"OK".to_vec(),
            b"OK".to_vec(),
            vec![0, 0xAA],
            b"OK".to_vec(),
            b"OK".to_vec(),
            vec![0],
            vec![0xAA],
        ]);
        let mut session = PrinterSession::new(transport, test_config());
        let image = RgbImage::new(8, 1);

        let first_error = session.print(&image).unwrap_err();
        assert!(matches!(first_error, PrintFailure::RetrySafe(_)));
        assert_eq!(session.state(), SessionState::Disconnected);

        session.print(&image).unwrap();

        let writes = handle.writes.lock().unwrap();
        assert_eq!(writes[0], set_density(Density::Normal));
        assert_eq!(writes[1], set_auto_shutdown(DEFAULT_AUTO_SHUTDOWN_MINUTES));
        assert_eq!(writes[2], query_status());
        assert_eq!(writes[3], set_density(Density::Normal));
        assert_eq!(writes[4], set_auto_shutdown(DEFAULT_AUTO_SHUTDOWN_MINUTES));
        assert_eq!(writes[5], query_status());
        assert_eq!(*handle.disconnects.lock().unwrap(), 1);
        assert_eq!(session.state(), SessionState::Ready);
    }

    #[test]
    fn closes_connection_when_auto_shutdown_is_rejected() {
        let (transport, handle) = MockTransport::new([b"OK".to_vec(), b"NO".to_vec()]);
        let mut session = PrinterSession::new(transport, test_config());

        let error = session.initialize().unwrap_err();

        assert!(format!("{error:#}").contains("printer rejected auto-shutdown command"));
        assert_eq!(session.state(), SessionState::Disconnected);
        assert_eq!(
            handle.writes.lock().unwrap().as_slice(),
            [
                set_density(Density::Normal),
                set_auto_shutdown(DEFAULT_AUTO_SHUTDOWN_MINUTES),
            ]
        );
        assert_eq!(*handle.disconnects.lock().unwrap(), 1);
    }

    #[test]
    fn sends_android_command_sequence_and_waits_for_ack() {
        let (transport, handle) =
            MockTransport::new([b"OK".to_vec(), b"OK".to_vec(), vec![0], vec![0xAA]]);
        let mut session = PrinterSession::new(transport, test_config());
        let mut image = RgbImage::new(8, 1);
        for (index, pixel) in image.pixels_mut().enumerate() {
            *pixel = if index % 2 == 0 {
                Rgb([0, 0, 0])
            } else {
                Rgb([255, 255, 255])
            };
        }

        let outcome = session.print(&image).unwrap();

        assert_eq!(session.state(), SessionState::Ready);
        assert_eq!(outcome.raster_bytes, 9);
        let writes = handle.writes.lock().unwrap();
        assert_eq!(writes.len(), 8);
        assert_eq!(writes[0], set_density(Density::Normal));
        assert_eq!(writes[1], set_auto_shutdown(DEFAULT_AUTO_SHUTDOWN_MINUTES));
        assert_eq!(writes[2], query_status());
        assert_eq!(writes[3], enable_printer());
        assert_eq!(writes[4], wake_printer());
        assert_eq!(
            writes[5],
            [vec![0x1D, 0x76, 0x30, 0, 1, 0, 1, 0], vec![0xAA]].concat()
        );
        assert_eq!(writes[6], feed_dots(DEFAULT_FEED_DOTS));
        assert_eq!(writes[7], stop_print_job());
    }

    #[test]
    fn keeps_ready_connection_when_status_blocks_printing() {
        let (transport, handle) =
            MockTransport::new([b"OK".to_vec(), b"OK".to_vec(), vec![0b0000_0100]]);
        let mut session = PrinterSession::new(transport, test_config());
        let image = RgbImage::new(8, 1);

        let error = session.print(&image).unwrap_err();

        assert!(matches!(error, PrintFailure::RetrySafe(_)));
        assert!(error.to_string().contains("not ready"));
        assert_eq!(session.state(), SessionState::Ready);
        assert_eq!(handle.writes.lock().unwrap().len(), 3);
        assert_eq!(*handle.disconnects.lock().unwrap(), 0);
    }

    #[test]
    fn rejects_multi_byte_status_responses() {
        let (transport, handle) =
            MockTransport::new([b"OK".to_vec(), b"OK".to_vec(), vec![0, 0xAA]]);
        let mut session = PrinterSession::new(transport, test_config());

        let error = session.print(&RgbImage::new(8, 1)).unwrap_err();

        assert!(matches!(error, PrintFailure::RetrySafe(_)));
        assert!(
            error
                .error()
                .to_string()
                .contains("expected exactly 1 byte")
        );
        assert_eq!(session.state(), SessionState::Disconnected);
        assert_eq!(*handle.disconnects.lock().unwrap(), 1);
    }

    #[test]
    fn rejects_unverified_async_stop_response() {
        let (transport, handle) = MockTransport::new([
            b"OK".to_vec(),
            b"OK".to_vec(),
            vec![0],
            vec![0xFF, 0x02],
            b"OK complete".to_vec(),
        ]);
        let mut session = PrinterSession::new(transport, test_config());

        let error = session.print(&RgbImage::new(8, 1)).unwrap_err();

        assert!(matches!(error, PrintFailure::OutcomeUnknown(_)));
        assert!(
            error
                .error()
                .to_string()
                .contains("failed while waiting for print completion")
        );
        assert_eq!(session.state(), SessionState::Disconnected);
        assert_eq!(*handle.disconnects.lock().unwrap(), 1);
    }

    #[test]
    fn reassembles_split_ok_responses() {
        let (transport, _) = MockTransport::new([
            b"O".to_vec(),
            b"K".to_vec(),
            b"O".to_vec(),
            b"K".to_vec(),
            vec![0],
            b"O".to_vec(),
            b"K complete".to_vec(),
        ]);
        let mut session = PrinterSession::new(transport, test_config());

        session.print(&RgbImage::new(8, 1)).unwrap();

        assert_eq!(session.state(), SessionState::Ready);
    }

    #[test]
    fn rejects_unknown_responses_without_silent_discard() {
        let (transport, handle) = MockTransport::new([b"NO".to_vec()]);
        let mut session = PrinterSession::new(transport, test_config());

        let error = session.print(&RgbImage::new(8, 1)).unwrap_err();

        assert!(matches!(error, PrintFailure::RetrySafe(_)));
        assert!(format!("{:#}", error.error()).contains("unexpected printer response"));
        assert_eq!(session.state(), SessionState::Disconnected);
        assert_eq!(*handle.disconnects.lock().unwrap(), 1);
    }

    #[test]
    fn failure_before_raster_write_is_retry_safe() {
        let (transport, handle) = MockTransport::new([b"OK".to_vec(), b"OK".to_vec(), vec![0]]);
        let mut session = PrinterSession::new(transport.failing_write_at(4), test_config());

        let error = session.print(&RgbImage::new(8, 1)).unwrap_err();

        assert!(matches!(error, PrintFailure::RetrySafe(_)));
        assert_eq!(session.state(), SessionState::Disconnected);
        assert_eq!(*handle.disconnects.lock().unwrap(), 1);
    }

    #[test]
    fn raster_write_failure_makes_outcome_unknown() {
        let (transport, handle) = MockTransport::new([b"OK".to_vec(), b"OK".to_vec(), vec![0]]);
        let mut session = PrinterSession::new(transport.failing_write_at(5), test_config());

        let error = session.print(&RgbImage::new(8, 1)).unwrap_err();

        assert!(matches!(error, PrintFailure::OutcomeUnknown(_)));
        assert_eq!(session.state(), SessionState::Disconnected);
        assert_eq!(*handle.disconnects.lock().unwrap(), 1);
    }

    #[test]
    fn stop_timeout_makes_outcome_unknown() {
        let (transport, handle) = MockTransport::new([b"OK".to_vec(), b"OK".to_vec(), vec![0]]);
        let mut config = test_config();
        config.stop_timeout = Duration::ZERO;
        let mut session = PrinterSession::new(transport, config);

        let error = session.print(&RgbImage::new(8, 1)).unwrap_err();

        assert!(matches!(error, PrintFailure::OutcomeUnknown(_)));
        assert!(format!("{:#}", error.error()).contains("printer response timeout"));
        assert_eq!(session.state(), SessionState::Disconnected);
        assert_eq!(*handle.disconnects.lock().unwrap(), 1);
    }

    #[test]
    fn stop_read_error_makes_outcome_unknown() {
        let (transport, handle) = MockTransport::with_results([
            Ok(b"OK".to_vec()),
            Ok(b"OK".to_vec()),
            Ok(vec![0]),
            Err(anyhow!("mock stop read failed")),
        ]);
        let mut session = PrinterSession::new(transport, test_config());

        let error = session.print(&RgbImage::new(8, 1)).unwrap_err();

        assert!(matches!(error, PrintFailure::OutcomeUnknown(_)));
        assert!(format!("{:#}", error.error()).contains("mock stop read failed"));
        assert_eq!(session.state(), SessionState::Disconnected);
        assert_eq!(*handle.disconnects.lock().unwrap(), 1);
    }
}
