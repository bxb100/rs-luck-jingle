use std::time::Duration;

#[cfg(test)]
use std::io::{self, Write};

#[cfg(target_os = "macos")]
use anyhow::Context;
use anyhow::{Result, bail};

pub const SPP_UUID: &str = "00001101-0000-1000-8000-00805F9B34FB";
pub const MAX_WRITE_CHUNK: usize = 16_384;

#[cfg(test)]
fn write_in_chunks<W>(writer: &mut W, data: &[u8]) -> io::Result<()>
where
    W: Write,
{
    for chunk in data.chunks(MAX_WRITE_CHUNK) {
        writer.write_all(chunk)?;
        writer.flush()?;
    }
    Ok(())
}

pub trait Transport: Send {
    fn connect(&mut self) -> Result<()>;
    fn is_connected(&self) -> bool;
    fn write_all(&mut self, data: &[u8]) -> Result<()>;
    fn read(&mut self, timeout: Duration) -> Result<Vec<u8>>;
    fn disconnect(&mut self) -> Result<()>;
}

pub struct RfcommTransport {
    #[cfg(not(target_os = "macos"))]
    address: String,
    #[cfg(not(target_os = "macos"))]
    channel: Option<u8>,
    #[cfg(target_os = "macos")]
    backend: crate::macos_bluetooth::MacRfcommBackend,
    #[cfg(target_os = "linux")]
    bdaddr: [u8; 6],
    #[cfg(target_os = "linux")]
    stream: Option<std::fs::File>,
    #[cfg(target_os = "linux")]
    profile: Option<linux::ProfileConnection>,
}

impl RfcommTransport {
    pub fn new(address: impl Into<String>, channel: u8) -> Result<Self> {
        let address = normalize_bdaddr(&address.into())?;
        let bdaddr = parse_bdaddr(&address)?;
        validate_channel(channel)?;

        #[cfg(not(target_os = "linux"))]
        let _ = bdaddr;

        #[cfg(target_os = "macos")]
        let backend =
            crate::macos_bluetooth::MacRfcommBackend::new(address.clone(), Some(channel))?;

        Ok(Self {
            #[cfg(not(target_os = "macos"))]
            address,
            #[cfg(not(target_os = "macos"))]
            channel: Some(channel),
            #[cfg(target_os = "macos")]
            backend,
            #[cfg(target_os = "linux")]
            bdaddr,
            #[cfg(target_os = "linux")]
            stream: None,
            #[cfg(target_os = "linux")]
            profile: None,
        })
    }

    pub async fn from_profile(address: impl Into<String>) -> Result<Self> {
        let address = normalize_bdaddr(&address.into())?;
        let bdaddr = parse_bdaddr(&address)?;

        #[cfg(target_os = "linux")]
        {
            let mut transport = Self {
                address,
                channel: None,
                bdaddr,
                stream: None,
                profile: None,
            };
            linux::initialize_profile(&mut transport).await?;
            Ok(transport)
        }

        #[cfg(target_os = "macos")]
        {
            let _ = bdaddr;
            tokio::task::spawn_blocking(move || {
                let mut backend =
                    crate::macos_bluetooth::MacRfcommBackend::new(address.clone(), None)?;
                backend.connect()?;
                Ok(Self { backend })
            })
            .await
            .context("macOS RFCOMM connection task failed")?
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = bdaddr;
            bail!(
                "RFCOMM profile discovery is unsupported on {} for {}",
                std::env::consts::OS,
                address
            )
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn unsupported<T>(&self, operation: &str) -> Result<T> {
        bail!(
            "RFCOMM {operation} is unsupported on {} for {}{}",
            std::env::consts::OS,
            self.address,
            self.channel
                .map(|channel| format!(" channel {channel}"))
                .unwrap_or_default()
        )
    }
}

impl Transport for RfcommTransport {
    #[cfg(target_os = "linux")]
    fn connect(&mut self) -> Result<()> {
        linux::connect(self)
    }

    #[cfg(target_os = "macos")]
    fn connect(&mut self) -> Result<()> {
        self.backend.connect()
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn connect(&mut self) -> Result<()> {
        self.unsupported("connect")
    }

    #[cfg(target_os = "linux")]
    fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    #[cfg(target_os = "macos")]
    fn is_connected(&self) -> bool {
        self.backend.is_connected()
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn is_connected(&self) -> bool {
        false
    }

    #[cfg(target_os = "linux")]
    fn write_all(&mut self, data: &[u8]) -> Result<()> {
        linux::write_all(self, data)
    }

    #[cfg(target_os = "macos")]
    fn write_all(&mut self, data: &[u8]) -> Result<()> {
        self.backend.write_all(data)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn write_all(&mut self, _data: &[u8]) -> Result<()> {
        self.unsupported("write")
    }

    #[cfg(target_os = "linux")]
    fn read(&mut self, timeout: Duration) -> Result<Vec<u8>> {
        linux::read(self, timeout)
    }

    #[cfg(target_os = "macos")]
    fn read(&mut self, timeout: Duration) -> Result<Vec<u8>> {
        self.backend.read(timeout)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn read(&mut self, _timeout: Duration) -> Result<Vec<u8>> {
        self.unsupported("read")
    }

    #[cfg(target_os = "linux")]
    fn disconnect(&mut self) -> Result<()> {
        linux::disconnect(self)
    }

    #[cfg(target_os = "macos")]
    fn disconnect(&mut self) -> Result<()> {
        self.backend.disconnect()
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn disconnect(&mut self) -> Result<()> {
        Ok(())
    }
}

fn parse_bdaddr(address: &str) -> Result<[u8; 6]> {
    let normalized = normalize_bdaddr(address)?;
    let parts: Vec<&str> = normalized.split(':').collect();
    let mut display_order = [0_u8; 6];
    for (index, part) in parts.iter().enumerate() {
        display_order[index] = u8::from_str_radix(part, 16)
            .map_err(|_| anyhow::anyhow!("invalid Bluetooth address: {address}"))?;
    }

    display_order.reverse();
    Ok(display_order)
}

fn normalize_bdaddr(address: &str) -> Result<String> {
    let separator = match (address.contains(':'), address.contains('-')) {
        (true, false) => ':',
        (false, true) => '-',
        _ => bail!("invalid Bluetooth address: {address}"),
    };
    let parts: Vec<&str> = address.split(separator).collect();
    if parts.len() != 6 || parts.iter().any(|part| part.len() != 2) {
        bail!("invalid Bluetooth address: {address}");
    }

    let mut normalized = String::with_capacity(17);
    for (index, part) in parts.iter().enumerate() {
        let octet = u8::from_str_radix(part, 16)
            .map_err(|_| anyhow::anyhow!("invalid Bluetooth address: {address}"))?;
        if index != 0 {
            normalized.push(':');
        }
        normalized.push_str(&format!("{octet:02X}"));
    }
    Ok(normalized)
}

fn validate_channel(channel: u8) -> Result<()> {
    if !(1..=30).contains(&channel) {
        bail!("invalid RFCOMM channel {channel}; expected 1..=30");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs::File;
    use std::future::Future;
    use std::io::{self, ErrorKind, Read, Write};
    use std::mem::size_of;
    use std::os::fd::{AsRawFd, FromRawFd, RawFd};
    use std::pin::Pin;
    use std::thread;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, bail};
    use bluer::rfcomm::{Profile, ProfileHandle, ReqError, Role};
    use bluer::{Address, Device, ErrorKind as BluerErrorKind, Session, Uuid};
    use futures_util::{FutureExt, StreamExt};
    use tokio::runtime::{Handle, RuntimeFlavor};

    use super::{RfcommTransport, SPP_UUID};

    const BTPROTO_RFCOMM: libc::c_int = 3;
    const SOL_BLUETOOTH: libc::c_int = 274;
    const BT_SECURITY: libc::c_int = 4;
    const BT_SECURITY_LOW: u8 = 1;
    const BT_SECURITY_MEDIUM: u8 = 2;
    const READ_BUFFER_SIZE: usize = 8_192;
    const INSECURE_FALLBACK_DELAY: Duration = Duration::from_millis(150);
    const PROFILE_FAILURE_SETTLE_DELAY: Duration = Duration::from_millis(150);
    const PROFILE_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum SecurityLevel {
        Medium,
        Low,
    }

    impl SecurityLevel {
        const fn kernel_value(self) -> u8 {
            match self {
                Self::Medium => BT_SECURITY_MEDIUM,
                Self::Low => BT_SECURITY_LOW,
            }
        }

        const fn label(self) -> &'static str {
            match self {
                Self::Medium => "medium",
                Self::Low => "low",
            }
        }
    }

    const DIRECT_SECURITY_SEQUENCE: [SecurityLevel; 2] =
        [SecurityLevel::Medium, SecurityLevel::Low];

    pub(super) struct ProfileConnection {
        profile: Pin<Box<ProfileHandle>>,
        device: Device,
        _session: Session,
        runtime: Handle,
        target: Address,
        uuid: Uuid,
        active: bool,
    }

    #[repr(C)]
    struct BdAddr {
        bytes: [u8; 6],
    }

    #[repr(C)]
    struct SockAddrRc {
        family: libc::sa_family_t,
        bdaddr: BdAddr,
        channel: u8,
    }

    #[repr(C)]
    struct BtSecurity {
        level: u8,
        key_size: u8,
    }

    pub(super) async fn initialize_profile(transport: &mut RfcommTransport) -> Result<()> {
        let runtime = Handle::try_current()
            .context("RFCOMM profile setup requires an active Tokio runtime")?;
        let target = transport
            .address
            .parse::<Address>()
            .with_context(|| format!("invalid Bluetooth address: {}", transport.address))?;
        let uuid = Uuid::parse_str(SPP_UUID).context("invalid SPP UUID constant")?;
        let session = Session::new()
            .await
            .context("failed to create BlueZ session")?;
        let adapter = session
            .default_adapter()
            .await
            .context("failed to select the default Bluetooth adapter")?;
        if !adapter
            .is_powered()
            .await
            .context("failed to read the Bluetooth adapter power state")?
        {
            adapter
                .set_powered(true)
                .await
                .context("failed to power on the Bluetooth adapter")?;
        }
        let device = adapter.device(target).with_context(|| {
            format!(
                "failed to access Bluetooth device {} on adapter {}",
                transport.address,
                adapter.name()
            )
        })?;
        let profile = session
            .register_profile(spp_client_profile(uuid))
            .await
            .context("failed to register the SPP client profile")?;
        let mut connection = ProfileConnection {
            profile: Box::pin(profile),
            device,
            _session: session,
            runtime,
            target,
            uuid,
            active: false,
        };

        let (stream, channel) = connect_profile(&mut connection).await?;
        transport.stream = Some(stream);
        transport.channel = Some(channel);
        transport.profile = Some(connection);
        Ok(())
    }

    fn spp_client_profile(uuid: Uuid) -> Profile {
        Profile {
            uuid,
            name: Some("Luck Jingle SPP client".to_owned()),
            role: Some(Role::Client),
            require_authentication: Some(false),
            require_authorization: Some(false),
            auto_connect: Some(false),
            ..Profile::default()
        }
    }

    pub(super) fn connect(transport: &mut RfcommTransport) -> Result<()> {
        if transport.stream.is_some() {
            return Ok(());
        }

        if transport.profile.is_some() {
            return reconnect_profile(transport);
        }

        connect_direct(transport)
    }

    fn connect_direct(transport: &mut RfcommTransport) -> Result<()> {
        let first_security = DIRECT_SECURITY_SEQUENCE[0];
        let fallback_security = DIRECT_SECURITY_SEQUENCE[1];
        let first_error = match connect_direct_once(transport, first_security) {
            Ok(stream) => {
                transport.stream = Some(stream);
                return Ok(());
            }
            Err(error) => error,
        };

        thread::sleep(INSECURE_FALLBACK_DELAY);
        match connect_direct_once(transport, fallback_security) {
            Ok(stream) => {
                transport.stream = Some(stream);
                Ok(())
            }
            Err(error) => Err(error).with_context(|| {
                format!(
                    "RFCOMM {}-security attempt failed before {}-security fallback: {first_error:#}",
                    first_security.label(),
                    fallback_security.label()
                )
            }),
        }
    }

    fn connect_direct_once(transport: &RfcommTransport, security: SecurityLevel) -> Result<File> {
        let channel = transport
            .channel
            .context("direct RFCOMM transport is missing its channel")?;

        let raw_fd = unsafe {
            libc::socket(
                libc::AF_BLUETOOTH,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                BTPROTO_RFCOMM,
            )
        };
        if raw_fd < 0 {
            return Err(std::io::Error::last_os_error()).context("failed to create RFCOMM socket");
        }

        let stream = unsafe { File::from_raw_fd(raw_fd) };
        set_security(stream.as_raw_fd(), security).with_context(|| {
            format!(
                "failed to request {} RFCOMM security for {} on channel {}",
                security.label(),
                transport.address,
                channel
            )
        })?;
        let socket_address = SockAddrRc {
            family: libc::AF_BLUETOOTH as libc::sa_family_t,
            bdaddr: BdAddr {
                bytes: transport.bdaddr,
            },
            channel,
        };

        let result = unsafe {
            libc::connect(
                stream.as_raw_fd(),
                (&socket_address as *const SockAddrRc).cast::<libc::sockaddr>(),
                size_of::<SockAddrRc>() as libc::socklen_t,
            )
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!(
                    "failed to connect to RFCOMM device {} on channel {} with {} security",
                    transport.address,
                    channel,
                    security.label()
                )
            });
        }

        Ok(stream)
    }

    fn set_security(raw_fd: RawFd, security: SecurityLevel) -> Result<()> {
        let option = BtSecurity {
            level: security.kernel_value(),
            key_size: 0,
        };
        let result = unsafe {
            libc::setsockopt(
                raw_fd,
                SOL_BLUETOOTH,
                BT_SECURITY,
                (&option as *const BtSecurity).cast::<libc::c_void>(),
                size_of::<BtSecurity>() as libc::socklen_t,
            )
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error()).context("BT_SECURITY setsockopt failed");
        }
        Ok(())
    }

    fn reconnect_profile(transport: &mut RfcommTransport) -> Result<()> {
        let connection = transport
            .profile
            .as_mut()
            .context("RFCOMM profile transport is not initialized")?;
        let runtime = connection.runtime.clone();
        let result = block_on_runtime(&runtime, connect_profile(connection))?;
        transport.stream = Some(result.0);
        transport.channel = Some(result.1);
        Ok(())
    }

    async fn connect_profile(connection: &mut ProfileConnection) -> Result<(File, u8)> {
        if connection.active {
            cleanup_profile(connection)
                .await
                .context("failed to clean up the previous SPP profile connection")?;
        }

        let device = connection.device.clone();
        let cleanup_device = connection.device.clone();
        let uuid = connection.uuid;
        let target = connection.target;
        let mut profile = connection.profile.as_mut();
        reject_queued_requests(profile.as_mut());

        let connect = async move {
            match device.connect_profile(&uuid).await {
                Ok(()) => Ok(()),
                Err(error) if connect_is_already_pending(&error.kind) => {
                    log::debug!(
                        "BlueZ reported an existing SPP connection attempt for {target}: {error}"
                    );
                    Ok(())
                }
                Err(error) => {
                    Err(error).with_context(|| format!("BlueZ ConnectProfile failed for {target}"))
                }
            }
        };
        let accept = async move {
            loop {
                let request = profile
                    .next()
                    .await
                    .context("SPP profile was unregistered while waiting for a connection")?;
                if request.device() == target {
                    return request
                        .accept()
                        .with_context(|| format!("failed to accept SPP connection from {target}"));
                }

                log::warn!(
                    "rejecting SPP profile connection from unexpected device {} while waiting for {}",
                    request.device(),
                    target
                );
                request.reject(ReqError::Rejected);
            }
        };

        let handshake = tokio::time::timeout(PROFILE_CONNECT_TIMEOUT, async {
            futures_util::try_join!(connect, accept)
        })
        .await;

        let stream = match handshake {
            Ok(Ok((_, stream))) => stream,
            Ok(Err(error)) => {
                best_effort_failed_connect_cleanup(connection, cleanup_device, uuid).await;
                return Err(error);
            }
            Err(error) => {
                best_effort_failed_connect_cleanup(connection, cleanup_device, uuid).await;
                return Err(error)
                    .with_context(|| format!("timed out connecting SPP profile to {target}"));
            }
        };

        let adoption = (|| {
            let channel = stream
                .peer_addr()
                .with_context(|| format!("failed to inspect SPP connection from {target}"))?
                .channel;
            let file = duplicate_as_blocking_file(stream.as_raw_fd())
                .with_context(|| format!("failed to adopt SPP connection from {target}"))?;
            Ok::<_, anyhow::Error>((file, channel))
        })();
        drop(stream);
        match adoption {
            Ok(established) => {
                connection.active = true;
                Ok(established)
            }
            Err(error) => {
                best_effort_failed_connect_cleanup(connection, cleanup_device, uuid).await;
                Err(error)
            }
        }
    }

    async fn best_effort_failed_connect_cleanup(
        connection: &mut ProfileConnection,
        device: Device,
        uuid: Uuid,
    ) {
        if let Err(error) = device.disconnect_profile(&uuid).await {
            log::warn!("failed to clean up an incomplete SPP connection: {error}");
        }
        connection.active = false;
        tokio::time::sleep(PROFILE_FAILURE_SETTLE_DELAY).await;
        reject_queued_requests(connection.profile.as_mut());
    }

    fn connect_is_already_pending(kind: &BluerErrorKind) -> bool {
        matches!(
            kind,
            BluerErrorKind::AlreadyConnected | BluerErrorKind::InProgress
        )
    }

    async fn cleanup_profile(connection: &mut ProfileConnection) -> Result<()> {
        let result = connection
            .device
            .disconnect_profile(&connection.uuid)
            .await
            .map_err(anyhow::Error::from)
            .context("BlueZ DisconnectProfile failed");
        reject_queued_requests(connection.profile.as_mut());
        connection.active = false;
        result
    }

    fn reject_queued_requests(mut profile: Pin<&mut ProfileHandle>) {
        loop {
            let Some(request) = profile.as_mut().next().now_or_never().flatten() else {
                break;
            };
            request.reject(ReqError::Rejected);
        }
    }

    fn block_on_runtime<F, T>(runtime: &Handle, future: F) -> Result<T>
    where
        F: Future<Output = Result<T>>,
    {
        match Handle::try_current() {
            Ok(current) if current.runtime_flavor() == RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| runtime.block_on(future))
            }
            Ok(_) => bail!("blocking RFCOMM operations require a multi-thread Tokio runtime"),
            Err(_) => runtime.block_on(future),
        }
    }

    fn duplicate_as_blocking_file(raw_fd: RawFd) -> Result<File> {
        let duplicate = unsafe { libc::fcntl(raw_fd, libc::F_DUPFD_CLOEXEC, 0) };
        if duplicate < 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to duplicate RFCOMM descriptor");
        }
        let file = unsafe { File::from_raw_fd(duplicate) };
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to read RFCOMM descriptor flags");
        }
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to make RFCOMM descriptor blocking");
        }
        Ok(file)
    }

    fn send_in_chunks(stream: &mut File, data: &[u8]) -> io::Result<()> {
        for chunk in data.chunks(super::MAX_WRITE_CHUNK) {
            let mut offset = 0;
            while offset < chunk.len() {
                let count = unsafe {
                    libc::send(
                        stream.as_raw_fd(),
                        chunk[offset..].as_ptr().cast::<libc::c_void>(),
                        chunk.len() - offset,
                        libc::MSG_NOSIGNAL,
                    )
                };
                if count < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() == ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                if count == 0 {
                    return Err(io::Error::new(
                        ErrorKind::WriteZero,
                        "RFCOMM send returned zero bytes",
                    ));
                }
                offset += count as usize;
            }
            stream.flush()?;
        }
        Ok(())
    }

    pub(super) fn write_all(transport: &mut RfcommTransport, data: &[u8]) -> Result<()> {
        let address = transport.address.clone();
        let endpoint = endpoint_description(transport);
        let result = transport
            .stream
            .as_mut()
            .with_context(|| format!("RFCOMM device {address}{endpoint} is not connected"))
            .and_then(|stream| {
                send_in_chunks(stream, data).with_context(|| {
                    format!(
                        "failed to write or flush {} bytes to RFCOMM device {address}{endpoint}",
                        data.len()
                    )
                })
            });
        if result.is_err() {
            best_effort_io_cleanup(transport);
        }
        result
    }

    pub(super) fn read(transport: &mut RfcommTransport, timeout: Duration) -> Result<Vec<u8>> {
        let address = transport.address.clone();
        let endpoint = endpoint_description(transport);
        let result = (|| {
            let raw_fd = transport
                .stream
                .as_ref()
                .with_context(|| format!("RFCOMM device {address}{endpoint} is not connected"))?
                .as_raw_fd();

            wait_until_readable(raw_fd, timeout).with_context(|| {
                format!("failed while waiting for RFCOMM device {address}{endpoint}")
            })?;

            let Some(stream) = transport.stream.as_mut() else {
                bail!("RFCOMM device {address}{endpoint} is not connected");
            };
            let mut buffer = vec![0_u8; READ_BUFFER_SIZE];
            let count = match stream.read(&mut buffer) {
                Ok(0) => bail!("RFCOMM device {address}{endpoint} closed the connection"),
                Ok(count) => count,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to read from RFCOMM device {address}{endpoint}")
                    });
                }
            };
            buffer.truncate(count);
            Ok(buffer)
        })();
        if result.is_err() {
            best_effort_io_cleanup(transport);
        }
        result
    }

    fn best_effort_io_cleanup(transport: &mut RfcommTransport) {
        if let Err(error) = disconnect(transport) {
            log::warn!("failed to clean up RFCOMM transport after an I/O error: {error:#}");
        }
    }

    pub(super) fn disconnect(transport: &mut RfcommTransport) -> Result<()> {
        transport.stream.take();

        let Some(connection) = transport.profile.as_mut() else {
            return Ok(());
        };
        if !connection.active {
            return Ok(());
        }
        let runtime = connection.runtime.clone();
        block_on_runtime(&runtime, cleanup_profile(connection))
    }

    fn endpoint_description(transport: &RfcommTransport) -> String {
        transport
            .channel
            .map(|channel| format!(" on channel {channel}"))
            .unwrap_or_else(|| " via an SDP-discovered channel".to_owned())
    }

    fn wait_until_readable(raw_fd: libc::c_int, timeout: Duration) -> Result<()> {
        let deadline = Instant::now().checked_add(timeout);
        let mut remaining = timeout;

        loop {
            let mut poll_fd = libc::pollfd {
                fd: raw_fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let result =
                unsafe { libc::poll(&mut poll_fd, 1, duration_to_poll_timeout(remaining)) };

            if result == 0 {
                bail!("timed out waiting for RFCOMM data");
            }
            if result < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    remaining = deadline
                        .and_then(|deadline| deadline.checked_duration_since(Instant::now()))
                        .unwrap_or(Duration::ZERO);
                    if remaining.is_zero() {
                        bail!("timed out waiting for RFCOMM data");
                    }
                    continue;
                }
                return Err(error).context("RFCOMM poll failed");
            }

            if poll_fd.revents & libc::POLLNVAL != 0 {
                bail!("RFCOMM poll reported an invalid socket");
            }
            if poll_fd.revents & libc::POLLERR != 0 {
                bail!("RFCOMM poll reported a socket error");
            }
            if poll_fd.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                return Ok(());
            }
        }
    }

    fn duration_to_poll_timeout(timeout: Duration) -> libc::c_int {
        if timeout.is_zero() {
            return 0;
        }

        let mut milliseconds = timeout.as_millis();
        if timeout.subsec_nanos() % 1_000_000 != 0 {
            milliseconds = milliseconds.saturating_add(1);
        }
        milliseconds.min(libc::c_int::MAX as u128) as libc::c_int
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn socket_address_matches_linux_abi_size() {
            assert_eq!(size_of::<SockAddrRc>(), 10);
        }

        #[test]
        fn poll_timeout_rounds_positive_sub_millisecond_values_up() {
            assert_eq!(duration_to_poll_timeout(Duration::ZERO), 0);
            assert_eq!(duration_to_poll_timeout(Duration::from_nanos(1)), 1);
            assert_eq!(duration_to_poll_timeout(Duration::from_millis(1)), 1);
        }

        #[test]
        fn spp_profile_uses_client_role_without_a_fixed_channel_or_authentication() {
            let uuid = Uuid::parse_str(SPP_UUID).unwrap();
            let profile = spp_client_profile(uuid);

            assert_eq!(profile.uuid, uuid);
            assert_eq!(profile.role, Some(Role::Client));
            assert_eq!(profile.channel, None);
            assert_eq!(profile.require_authentication, Some(false));
            assert_eq!(profile.require_authorization, Some(false));
            assert_eq!(profile.auto_connect, Some(false));
        }

        #[test]
        fn direct_connection_tries_medium_security_before_low_security() {
            assert_eq!(
                DIRECT_SECURITY_SEQUENCE,
                [SecurityLevel::Medium, SecurityLevel::Low]
            );
            assert_eq!(INSECURE_FALLBACK_DELAY, Duration::from_millis(150));
        }

        #[test]
        fn existing_profile_connection_states_wait_for_new_connection() {
            assert!(connect_is_already_pending(
                &BluerErrorKind::AlreadyConnected
            ));
            assert!(connect_is_already_pending(&BluerErrorKind::InProgress));
            assert!(!connect_is_already_pending(
                &BluerErrorKind::AuthenticationFailed
            ));
        }

        #[test]
        fn duplicated_descriptor_is_blocking_and_close_on_exec() {
            let (source, _peer) = socket_pair(libc::SOCK_NONBLOCK);
            let duplicate = duplicate_as_blocking_file(source.as_raw_fd()).unwrap();
            let status_flags = unsafe { libc::fcntl(duplicate.as_raw_fd(), libc::F_GETFL) };
            let descriptor_flags = unsafe { libc::fcntl(duplicate.as_raw_fd(), libc::F_GETFD) };

            assert_eq!(status_flags & libc::O_NONBLOCK, 0);
            assert_ne!(descriptor_flags & libc::FD_CLOEXEC, 0);
        }

        #[test]
        fn send_uses_no_signal_and_clears_a_broken_stream() {
            let (stream, peer) = socket_pair(0);
            drop(peer);
            let mut transport = direct_transport(stream);

            assert!(write_all(&mut transport, b"payload").is_err());
            assert!(transport.stream.is_none());
        }

        #[test]
        fn eof_clears_a_broken_stream_for_reconnect() {
            let (stream, peer) = socket_pair(0);
            drop(peer);
            let mut transport = direct_transport(stream);

            assert!(read(&mut transport, Duration::from_millis(10)).is_err());
            assert!(transport.stream.is_none());
        }

        fn socket_pair(flags: libc::c_int) -> (File, File) {
            let mut descriptors = [-1; 2];
            let result = unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_STREAM | libc::SOCK_CLOEXEC | flags,
                    0,
                    descriptors.as_mut_ptr(),
                )
            };
            assert_eq!(
                result,
                0,
                "socketpair failed: {}",
                std::io::Error::last_os_error()
            );
            unsafe {
                (
                    File::from_raw_fd(descriptors[0]),
                    File::from_raw_fd(descriptors[1]),
                )
            }
        }

        fn direct_transport(stream: File) -> RfcommTransport {
            RfcommTransport {
                address: "02:00:00:00:00:01".to_owned(),
                channel: Some(1),
                bdaddr: [1, 0, 0, 0, 0, 2],
                stream: Some(stream),
                profile: None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PartialWriter {
        max_write: usize,
        bytes: Vec<u8>,
        flushed_at: Vec<usize>,
    }

    impl Write for PartialWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            let count = buffer.len().min(self.max_write);
            self.bytes.extend_from_slice(&buffer[..count]);
            Ok(count)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushed_at.push(self.bytes.len());
            Ok(())
        }
    }

    #[test]
    fn protocol_constants_match_classic_spp() {
        assert_eq!(SPP_UUID, "00001101-0000-1000-8000-00805F9B34FB");
        assert_eq!(MAX_WRITE_CHUNK, 16_384);
    }

    #[test]
    fn logical_writes_handle_partial_writes_and_flush_each_chunk() {
        let data: Vec<u8> = (0..MAX_WRITE_CHUNK * 2 + 17)
            .map(|index| index as u8)
            .collect();
        let mut writer = PartialWriter {
            max_write: 1_001,
            bytes: Vec::new(),
            flushed_at: Vec::new(),
        };

        write_in_chunks(&mut writer, &data).unwrap();

        assert_eq!(writer.bytes, data);
        assert_eq!(
            writer.flushed_at,
            [
                MAX_WRITE_CHUNK,
                MAX_WRITE_CHUNK * 2,
                MAX_WRITE_CHUNK * 2 + 17
            ]
        );
    }

    #[test]
    fn address_is_reversed_for_linux_bdaddr_layout() {
        assert_eq!(
            parse_bdaddr("12:34:56:78:9a:bc").unwrap(),
            [0xbc, 0x9a, 0x78, 0x56, 0x34, 0x12]
        );
    }

    #[test]
    fn uppercase_address_is_accepted() {
        assert_eq!(
            parse_bdaddr("AA:BB:CC:DD:EE:FF").unwrap(),
            [0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa]
        );
    }

    #[test]
    fn macos_hyphenated_address_is_normalized() {
        assert_eq!(
            normalize_bdaddr("aa-bb-cc-dd-ee-ff").unwrap(),
            "AA:BB:CC:DD:EE:FF"
        );
        assert_eq!(
            parse_bdaddr("AA-BB-CC-DD-EE-FF").unwrap(),
            [0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa]
        );
    }

    #[test]
    fn malformed_addresses_are_rejected() {
        for address in [
            "",
            "12:34:56:78:9a",
            "12:34:56:78:9a:bc:de",
            "1:34:56:78:9a:bc",
            "12:34-56:78-9a:bc",
            "12:34:56:78:9a:gg",
        ] {
            assert!(parse_bdaddr(address).is_err(), "accepted {address}");
        }
    }

    #[test]
    fn invalid_channels_are_rejected() {
        assert!(RfcommTransport::new("12:34:56:78:9a:bc", 0).is_err());
        assert!(RfcommTransport::new("12:34:56:78:9a:bc", 31).is_err());
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[test]
    fn unsupported_platform_reports_actionable_errors() {
        let mut transport = RfcommTransport::new("12:34:56:78:9a:bc", 1).unwrap();
        assert!(!transport.is_connected());
        assert!(
            transport
                .connect()
                .unwrap_err()
                .to_string()
                .contains("unsupported")
        );
        assert!(
            transport
                .write_all(&[0x10])
                .unwrap_err()
                .to_string()
                .contains("unsupported")
        );
        assert!(
            transport
                .read(Duration::from_millis(1))
                .unwrap_err()
                .to_string()
                .contains("unsupported")
        );
        transport.disconnect().unwrap();
    }
}
