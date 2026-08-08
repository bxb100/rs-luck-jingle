use std::collections::{BTreeMap, VecDeque};
use std::env;
use std::ffi::{OsStr, c_void};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use objc2::rc::{Retained, autoreleasepool};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_foundation::{
    NSArray, NSDate, NSDefaultRunLoopMode, NSObject, NSObjectProtocol, NSRunLoop, NSString,
    NSThread,
};
use objc2_io_bluetooth::{
    BluetoothHCIPageTimeout, IOBluetoothDevice, IOBluetoothDeviceAsyncCallbacks,
    IOBluetoothDeviceInquiry, IOBluetoothDeviceInquiryDelegate, IOBluetoothDeviceSearchTypesBits,
    IOBluetoothRFCOMMChannel, IOBluetoothRFCOMMChannelDelegate, IOBluetoothSDPUUID,
};

use crate::transport::MAX_WRITE_CHUNK;

const SPP_UUID_16: u16 = 0x1101;
const MAX_INQUIRY_SECONDS: u64 = 48;
const RUN_LOOP_SLICE: Duration = Duration::from_millis(10);
const INQUIRY_STOP_GRACE: Duration = Duration::from_secs(2);
const REMOTE_NAME_TOTAL_TIMEOUT: Duration = Duration::from_secs(15);
const REMOTE_NAME_MAX_PAGE_TIMEOUT: BluetoothHCIPageTimeout = 0x2710;
const HCI_SLOT_MICROS: u128 = 625;
const SDP_TIMEOUT: Duration = Duration::from_secs(30);
const HELPER_MODE_ENV: &str = "RS_LUCK_JINGLE_MACOS_RFCOMM_HELPER";
const HELPER_MODE_VALUE: &str = "1";
const HELPER_START_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_REPLY_TIMEOUT: Duration = Duration::from_secs(45);
const WRITE_REPLY_TIMEOUT: Duration = Duration::from_secs(120);
const DISCONNECT_REPLY_TIMEOUT: Duration = Duration::from_secs(10);
const IS_CONNECTED_REPLY_TIMEOUT: Duration = Duration::from_secs(2);
const HELPER_REPLY_GRACE: Duration = Duration::from_secs(2);
const DROP_REPLY_TIMEOUT: Duration = Duration::from_millis(250);
const HELPER_EXIT_GRACE: Duration = Duration::from_millis(500);
const MAX_HELPER_READ_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_HELPER_WRITE_PAYLOAD: usize = 1024 * 1024;
const MAX_HELPER_ERROR_BYTES: usize = 16 * 1024;
const MAX_HELPER_REQUEST_PAYLOAD: usize = 1 + MAX_HELPER_WRITE_PAYLOAD;
const READ_BUFFER_SIZE: usize = 8_192;
const MAX_HELPER_RESPONSE_BODY: usize = if READ_BUFFER_SIZE > MAX_HELPER_ERROR_BYTES {
    READ_BUFFER_SIZE
} else {
    MAX_HELPER_ERROR_BYTES
};
const MAX_HELPER_RESPONSE_PAYLOAD: usize = 2 + MAX_HELPER_RESPONSE_BODY;
const CALLBACK_PENDING: i32 = i32::MIN;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DiscoveredDevice {
    pub name: Option<String>,
    pub address: String,
}

#[derive(Default)]
struct InquiryCompletion {
    started: AtomicBool,
    complete: AtomicBool,
    status: AtomicI32,
    aborted: AtomicBool,
    device_found_callbacks: AtomicUsize,
    name_updates_started: AtomicBool,
    name_update_callbacks: AtomicUsize,
    devices_remaining: AtomicU32,
    observed_devices: Mutex<BTreeMap<String, Option<String>>>,
}

impl InquiryCompletion {
    fn mark_started(&self) {
        self.started.store(true, Ordering::Release);
    }

    fn observe_device(&self, device: Option<&IOBluetoothDevice>) {
        let Some(device) = device else {
            return;
        };
        let Some((address, name)) = device_identity(device) else {
            return;
        };
        let mut devices = self
            .observed_devices
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        devices
            .entry(address)
            .and_modify(|current| {
                *current = latest_name(current.take(), name.clone());
            })
            .or_insert(name);
    }

    fn observed_devices(&self) -> BTreeMap<String, Option<String>> {
        self.observed_devices
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

struct InquiryDelegateIvars {
    completion: Arc<InquiryCompletion>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = AnyThread]
    #[name = "RsLuckJingleInquiryDelegate"]
    #[ivars = InquiryDelegateIvars]
    struct InquiryDelegate;

    unsafe impl NSObjectProtocol for InquiryDelegate {}

    #[allow(non_snake_case)]
    unsafe impl IOBluetoothDeviceInquiryDelegate for InquiryDelegate {
        #[unsafe(method(deviceInquiryStarted:))]
        fn deviceInquiryStarted(&self, _sender: Option<&IOBluetoothDeviceInquiry>) {
            self.ivars().completion.mark_started();
        }

        #[unsafe(method(deviceInquiryDeviceFound:device:))]
        fn deviceInquiryDeviceFound_device(
            &self,
            _sender: Option<&IOBluetoothDeviceInquiry>,
            device: Option<&IOBluetoothDevice>,
        ) {
            let completion = &self.ivars().completion;
            completion.mark_started();
            completion
                .device_found_callbacks
                .fetch_add(1, Ordering::Relaxed);
            completion.observe_device(device);
        }

        #[unsafe(method(deviceInquiryUpdatingDeviceNamesStarted:devicesRemaining:))]
        fn deviceInquiryUpdatingDeviceNamesStarted_devicesRemaining(
            &self,
            _sender: Option<&IOBluetoothDeviceInquiry>,
            devices_remaining: u32,
        ) {
            let completion = &self.ivars().completion;
            completion.mark_started();
            completion
                .name_updates_started
                .store(true, Ordering::Release);
            completion
                .devices_remaining
                .store(devices_remaining, Ordering::Release);
        }

        #[unsafe(method(deviceInquiryDeviceNameUpdated:device:devicesRemaining:))]
        fn deviceInquiryDeviceNameUpdated_device_devicesRemaining(
            &self,
            _sender: Option<&IOBluetoothDeviceInquiry>,
            device: Option<&IOBluetoothDevice>,
            devices_remaining: u32,
        ) {
            let completion = &self.ivars().completion;
            completion.mark_started();
            completion
                .name_update_callbacks
                .fetch_add(1, Ordering::Relaxed);
            completion
                .devices_remaining
                .store(devices_remaining, Ordering::Release);
            completion.observe_device(device);
        }

        #[unsafe(method(deviceInquiryComplete:error:aborted:))]
        fn deviceInquiryComplete_error_aborted(
            &self,
            _sender: Option<&IOBluetoothDeviceInquiry>,
            status: i32,
            aborted: bool,
        ) {
            let completion = &self.ivars().completion;
            completion.status.store(status, Ordering::Release);
            completion.aborted.store(aborted, Ordering::Release);
            completion.complete.store(true, Ordering::Release);
        }
    }
);

impl InquiryDelegate {
    fn new(completion: Arc<InquiryCompletion>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(InquiryDelegateIvars { completion });
        unsafe { msg_send![super(this), init] }
    }
}

pub(crate) fn discover_devices(timeout: Duration) -> Result<Vec<DiscoveredDevice>> {
    if timeout.is_zero() {
        bail!("Bluetooth discovery timeout must be greater than zero");
    }
    if !NSThread::isMainThread_class() {
        bail!("macOS Bluetooth Classic discovery must run on the process main thread");
    }

    let completion = Arc::new(InquiryCompletion::default());
    // The inquiry is autoreleased and does not retain its delegate, so the
    // delegate must outlive the pool drain as well as the active inquiry.
    let delegate = InquiryDelegate::new(Arc::clone(&completion));
    autoreleasepool(|_| discover_devices_inner(timeout, &completion, &delegate))
}

fn discover_devices_inner(
    timeout: Duration,
    completion: &InquiryCompletion,
    delegate: &Retained<InquiryDelegate>,
) -> Result<Vec<DiscoveredDevice>> {
    let inquiry = unsafe { IOBluetoothDeviceInquiry::inquiryWithDelegate(Some(&**delegate)) }
        .context("failed to create a macOS Bluetooth Classic inquiry")?;

    unsafe {
        inquiry.setSearchType(IOBluetoothDeviceSearchTypesBits::Classic.0);
        inquiry.setInquiryLength(inquiry_seconds(timeout));
        inquiry.setUpdateNewDeviceNames(false);
    }

    let start_status = unsafe { inquiry.start() };
    if start_status != 0 {
        unsafe {
            inquiry.setDelegate(None);
        }
        return Err(io_return_error(
            "failed to start macOS Bluetooth Classic inquiry; ensure Bluetooth is enabled and grant the launching app (such as Codex or Terminal) access in System Settings > Privacy & Security > Bluetooth",
            start_status,
        ));
    }

    let run_loop = NSRunLoop::currentRunLoop();
    let mut inquiry_deadline = deadline_after(timeout);
    let mut activity_observed = false;
    while !completion.complete.load(Ordering::Acquire) {
        if !activity_observed && completion.started.load(Ordering::Acquire) {
            activity_observed = true;
            inquiry_deadline = deadline_after(timeout);
        }
        let Some(remaining) = inquiry_deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        pump_run_loop(&run_loop, remaining.min(RUN_LOOP_SLICE));
    }

    let stopped_for_timeout = !completion.complete.load(Ordering::Acquire);
    if stopped_for_timeout {
        unsafe {
            let _ = inquiry.stop();
        }
        let stop_deadline = deadline_after(INQUIRY_STOP_GRACE);
        while !completion.complete.load(Ordering::Acquire) {
            let Some(remaining) = stop_deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            pump_run_loop(&run_loop, remaining.min(RUN_LOOP_SLICE));
        }
    }

    if !completion.complete.load(Ordering::Acquire) {
        std::mem::forget(delegate.clone());
        std::mem::forget(inquiry);
        bail!("macOS Bluetooth inquiry did not stop cleanly before its safety deadline");
    }

    let completion_status = completion.status.load(Ordering::Acquire);
    if completion_status != 0 {
        unsafe {
            inquiry.setDelegate(None);
        }
        return Err(io_return_error(
            "macOS Bluetooth Classic inquiry failed",
            completion_status,
        ));
    }
    unsafe {
        inquiry.setDelegate(None);
    }

    let found = match unsafe { inquiry.foundDevices() } {
        Some(found) => found.to_vec(),
        None => {
            log::debug!("macOS Bluetooth inquiry returned no final device collection");
            Vec::new()
        }
    };
    let mut observed_devices = completion.observed_devices();
    log::debug!(
        "macOS Bluetooth inquiry callbacks: started={}, complete={}, aborted={}, status=0x{:08X}, device_found={}, name_updates_started={}, name_updated={}, devices_remaining={}, found_collection={}",
        completion.started.load(Ordering::Acquire),
        completion.complete.load(Ordering::Acquire),
        completion.aborted.load(Ordering::Acquire),
        completion.status.load(Ordering::Acquire) as u32,
        completion.device_found_callbacks.load(Ordering::Acquire),
        completion.name_updates_started.load(Ordering::Acquire),
        completion.name_update_callbacks.load(Ordering::Acquire),
        completion.devices_remaining.load(Ordering::Acquire),
        found.len(),
    );

    let mut devices = Vec::new();
    let remote_name_deadline = deadline_after(REMOTE_NAME_TOTAL_TIMEOUT);
    for object in found {
        let Ok(device) = object.downcast::<IOBluetoothDevice>() else {
            log::debug!("macOS Bluetooth inquiry returned a non-device object");
            continue;
        };
        let Some(address) = (unsafe { device.addressString() }) else {
            log::debug!("macOS Bluetooth inquiry returned a device without an address");
            continue;
        };
        let raw_address = address.to_string();
        let Ok(address) = normalize_address(&raw_address) else {
            log::debug!("macOS Bluetooth inquiry returned invalid address {raw_address:?}");
            continue;
        };
        let observed_name = observed_devices.remove(&address).flatten();
        let page_timeout = remote_name_deadline
            .checked_duration_since(Instant::now())
            .and_then(remote_name_page_timeout);
        let name = resolve_device_name(&device, &address, observed_name, page_timeout);
        devices.push(DiscoveredDevice { name, address });
    }

    devices.extend(
        observed_devices
            .into_iter()
            .map(|(address, name)| DiscoveredDevice { name, address }),
    );
    let devices = normalize_discovered_devices(devices);
    let named_count = devices
        .iter()
        .filter(|device| device.name.is_some())
        .count();
    log::debug!(
        "macOS Bluetooth inquiry retained {} raw devices: {} named, {} unnamed",
        devices.len(),
        named_count,
        devices.len().saturating_sub(named_count),
    );
    for device in &devices {
        log::debug!(
            "macOS Bluetooth inquiry device {} name={:?}",
            device.address,
            device.name.as_deref()
        );
    }

    Ok(devices)
}

struct SdpDelegateIvars {
    status: Arc<AtomicI32>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = AnyThread]
    #[name = "RsLuckJingleSdpDelegate"]
    #[ivars = SdpDelegateIvars]
    struct SdpDelegate;

    unsafe impl NSObjectProtocol for SdpDelegate {}

    #[allow(non_snake_case)]
    unsafe impl IOBluetoothDeviceAsyncCallbacks for SdpDelegate {
        #[unsafe(method(remoteNameRequestComplete:status:))]
        fn remoteNameRequestComplete_status(
            &self,
            _device: Option<&IOBluetoothDevice>,
            _status: i32,
        ) {
        }

        #[unsafe(method(connectionComplete:status:))]
        fn connectionComplete_status(&self, _device: Option<&IOBluetoothDevice>, _status: i32) {}

        #[unsafe(method(sdpQueryComplete:status:))]
        fn sdpQueryComplete_status(&self, _device: Option<&IOBluetoothDevice>, status: i32) {
            self.ivars().status.store(status, Ordering::Release);
        }
    }
);

impl SdpDelegate {
    fn new(status: Arc<AtomicI32>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(SdpDelegateIvars { status });
        unsafe { msg_send![super(this), init] }
    }
}

#[derive(Default)]
struct ReadBuffer {
    bytes: VecDeque<u8>,
    closed_reason: Option<String>,
}

#[derive(Default)]
struct SharedReadState {
    connected: AtomicBool,
    buffer: Mutex<ReadBuffer>,
}

impl SharedReadState {
    fn mark_connected(&self) {
        let mut buffer = self
            .buffer
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        buffer.bytes.clear();
        buffer.closed_reason = None;
        self.connected.store(true, Ordering::Release);
    }

    fn append(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let mut buffer = self
            .buffer
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        buffer.bytes.extend(data.iter().copied());
    }

    fn mark_disconnected(&self, reason: impl Into<String>) {
        let mut buffer = self
            .buffer
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        buffer.closed_reason = Some(reason.into());
        self.connected.store(false, Ordering::Release);
    }

    fn take_available(&self) -> Result<Option<Vec<u8>>> {
        let mut buffer = self
            .buffer
            .lock()
            .map_err(|_| anyhow!("macOS RFCOMM receive buffer lock was poisoned"))?;
        if !buffer.bytes.is_empty() {
            let count = buffer.bytes.len().min(READ_BUFFER_SIZE);
            return Ok(Some(buffer.bytes.drain(..count).collect()));
        }
        if !self.connected.load(Ordering::Acquire) {
            let reason = buffer
                .closed_reason
                .as_deref()
                .unwrap_or("RFCOMM device is not connected");
            bail!("{reason}");
        }
        Ok(None)
    }
}

struct ChannelDelegateIvars {
    read_state: Arc<SharedReadState>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = AnyThread]
    #[name = "RsLuckJingleRfcommDelegate"]
    #[ivars = ChannelDelegateIvars]
    struct ChannelDelegate;

    unsafe impl NSObjectProtocol for ChannelDelegate {}

    #[allow(non_snake_case)]
    unsafe impl IOBluetoothRFCOMMChannelDelegate for ChannelDelegate {
        #[unsafe(method(rfcommChannelData:data:length:))]
        fn rfcommChannelData_data_length(
            &self,
            _channel: Option<&IOBluetoothRFCOMMChannel>,
            data: *mut c_void,
            length: usize,
        ) {
            if data.is_null() || length == 0 {
                return;
            }
            let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), length) };
            self.ivars().read_state.append(bytes);
        }

        #[unsafe(method(rfcommChannelClosed:))]
        fn rfcommChannelClosed(&self, _channel: Option<&IOBluetoothRFCOMMChannel>) {
            self.ivars()
                .read_state
                .mark_disconnected("RFCOMM channel was closed by the remote device");
        }
    }
);

impl ChannelDelegate {
    fn new(read_state: Arc<SharedReadState>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ChannelDelegateIvars { read_state });
        unsafe { msg_send![super(this), init] }
    }
}

struct WorkerContext {
    address: String,
    requested_channel: Option<u8>,
    read_state: Arc<SharedReadState>,
    channel: Option<Retained<IOBluetoothRFCOMMChannel>>,
    device: Option<Retained<IOBluetoothDevice>>,
    run_loop: Retained<NSRunLoop>,
    sdp_status: Option<Arc<AtomicI32>>,
    sdp_delegate: Option<Retained<SdpDelegate>>,
    delegate: Retained<ChannelDelegate>,
}

impl WorkerContext {
    fn new(
        address: String,
        requested_channel: Option<u8>,
        read_state: Arc<SharedReadState>,
    ) -> Self {
        let delegate = ChannelDelegate::new(Arc::clone(&read_state));
        Self {
            address,
            requested_channel,
            read_state,
            channel: None,
            device: None,
            run_loop: NSRunLoop::currentRunLoop(),
            sdp_status: None,
            sdp_delegate: None,
            delegate,
        }
    }

    fn connect(&mut self) -> Result<()> {
        if self
            .channel
            .as_ref()
            .is_some_and(|channel| unsafe { channel.isOpen() })
        {
            return Ok(());
        }

        self.disconnect_best_effort();
        let result = self.connect_inner();
        if result.is_err() {
            self.disconnect_best_effort();
        }
        result
    }

    fn connect_inner(&mut self) -> Result<()> {
        let address = NSString::from_str(&self.address);
        let device = unsafe { IOBluetoothDevice::deviceWithAddressString(Some(&address)) }
            .with_context(|| format!("failed to create macOS Bluetooth device {}", self.address))?;
        self.device = Some(device.clone());
        let connection_status = unsafe { device.openConnection_(None) };
        if connection_status != 0 && !unsafe { device.isConnected() } {
            return Err(io_return_error(
                &format!("failed to open a Bluetooth connection to {}", self.address),
                connection_status,
            ));
        }
        if connection_status != 0 {
            log::debug!(
                "macOS openConnection returned IOReturn 0x{:08X} for an already connected device {}",
                connection_status as u32,
                self.address
            );
        }
        let channel_id = match self.requested_channel {
            Some(channel) => channel,
            None => self.discover_spp_channel(&device)?,
        };

        let mut channel = None;
        let status = unsafe {
            device.openRFCOMMChannelSync_withChannelID_delegate(
                Some(&mut channel),
                channel_id,
                Some(&self.delegate),
            )
        };
        if status != 0 {
            return Err(io_return_error(
                &format!(
                    "failed to open RFCOMM channel {channel_id} for {}",
                    self.address
                ),
                status,
            ));
        }
        let channel = channel.with_context(|| {
            format!(
                "macOS reported success opening RFCOMM channel {channel_id} for {} without returning a channel",
                self.address
            )
        })?;
        if !unsafe { channel.isOpen() } {
            bail!(
                "RFCOMM channel {channel_id} for {} is not open after macOS reported success",
                self.address
            );
        }

        self.channel = Some(channel);
        self.read_state.mark_connected();
        Ok(())
    }

    fn discover_spp_channel(&mut self, device: &IOBluetoothDevice) -> Result<u8> {
        let uuid = unsafe { IOBluetoothSDPUUID::uuid16(SPP_UUID_16) }
            .context("failed to create the Bluetooth SPP UUID")?;
        let uuid_array = NSArray::<IOBluetoothSDPUUID>::from_slice(&[&uuid]);
        let uuid_array = unsafe { uuid_array.cast_unchecked() };
        self.preserve_pending_sdp_delegate();
        let callback_status = Arc::new(AtomicI32::new(CALLBACK_PENDING));
        self.sdp_status = Some(Arc::clone(&callback_status));
        self.sdp_delegate = Some(SdpDelegate::new(Arc::clone(&callback_status)));
        let delegate = self
            .sdp_delegate
            .as_ref()
            .context("failed to retain the SPP service discovery delegate")?;
        let start_status =
            unsafe { device.performSDPQuery_uuids(Some(delegate), Some(uuid_array)) };
        if start_status != 0 {
            return Err(io_return_error(
                &format!("failed to start SPP service discovery for {}", self.address),
                start_status,
            ));
        }

        let deadline = deadline_after(SDP_TIMEOUT);
        let (status, record) = loop {
            let record = unsafe { device.getServiceRecordForUUID(Some(&uuid)) };
            if record.is_some() {
                break (0, record);
            }
            let status = callback_status.load(Ordering::Acquire);
            if status != CALLBACK_PENDING {
                break (status, None);
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                let record = unsafe { device.getServiceRecordForUUID(Some(&uuid)) };
                if record.is_some() {
                    break (0, record);
                }
                // IOBluetooth does not retain SDP callback targets. The query cannot be
                // cancelled, so a timed-out target must stay alive for the process lifetime.
                if let Some(delegate) = self.sdp_delegate.take() {
                    std::mem::forget(delegate);
                }
                self.sdp_status.take();
                bail!("timed out discovering the SPP service for {}", self.address);
            };
            pump_run_loop(&self.run_loop, remaining.min(RUN_LOOP_SLICE));
        };
        if status != 0 {
            return Err(io_return_error(
                &format!("SPP service discovery failed for {}", self.address),
                status,
            ));
        }

        let record = record
            .or_else(|| unsafe { device.getServiceRecordForUUID(Some(&uuid)) })
            .with_context(|| {
                format!("device {} does not advertise the SPP service", self.address)
            })?;
        let mut channel_id = 0_u8;
        let status = unsafe { record.getRFCOMMChannelID(&mut channel_id) };
        if status != 0 {
            return Err(io_return_error(
                &format!("failed to read the SPP RFCOMM channel for {}", self.address),
                status,
            ));
        }
        validate_channel(channel_id)?;
        Ok(channel_id)
    }

    fn write_all(&mut self, data: &[u8]) -> Result<()> {
        let channel = self
            .channel
            .as_ref()
            .with_context(|| format!("RFCOMM device {} is not connected", self.address))?;
        if !unsafe { channel.isOpen() } {
            self.read_state
                .mark_disconnected("RFCOMM channel is no longer open");
            bail!("RFCOMM device {} is not connected", self.address);
        }

        let chunk_size = effective_write_chunk(unsafe { channel.getMTU() } as usize)?;
        for chunk in data.chunks(chunk_size) {
            let status = unsafe {
                channel.writeSync_length(
                    chunk.as_ptr().cast_mut().cast::<c_void>(),
                    chunk.len() as u16,
                )
            };
            if status != 0 {
                self.read_state.mark_disconnected(format!(
                    "RFCOMM write failed for {} with IOReturn 0x{:08X}",
                    self.address, status as u32
                ));
                return Err(io_return_error(
                    &format!(
                        "failed to write {} bytes to RFCOMM device {}",
                        chunk.len(),
                        self.address
                    ),
                    status,
                ));
            }
        }
        Ok(())
    }

    fn read(&mut self, timeout: Duration) -> Result<Vec<u8>> {
        let deadline = deadline_after(timeout);
        loop {
            if let Some(data) = self.read_state.take_available()? {
                return Ok(data);
            }

            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                bail!("timed out waiting for RFCOMM data");
            };
            autoreleasepool(|_| {
                pump_run_loop(&self.run_loop, remaining.min(RUN_LOOP_SLICE));
            });
        }
    }

    fn is_connected(&self) -> bool {
        self.read_state.connected.load(Ordering::Acquire)
            && self
                .channel
                .as_ref()
                .is_some_and(|channel| unsafe { channel.isOpen() })
    }

    fn disconnect(&mut self) -> Result<()> {
        self.read_state
            .mark_disconnected(format!("RFCOMM device {} is disconnected", self.address));
        let mut first_error = None;

        if let Some(channel) = self.channel.take() {
            unsafe {
                let unset_status = channel.setDelegate(None);
                if unset_status != 0 {
                    std::mem::forget(self.delegate.clone());
                    first_error = Some(io_return_error(
                        &format!("failed to clear RFCOMM delegate for {}", self.address),
                        unset_status,
                    ));
                }
                if channel.isOpen() {
                    let status = channel.closeChannel();
                    if status != 0 && first_error.is_none() {
                        first_error = Some(io_return_error(
                            &format!("failed to close RFCOMM channel for {}", self.address),
                            status,
                        ));
                    }
                }
            }
        }

        if let Some(device) = self.device.take()
            && unsafe { device.isConnected() }
        {
            let status = unsafe { device.closeConnection() };
            if status != 0 && first_error.is_none() {
                first_error = Some(io_return_error(
                    &format!("failed to close Bluetooth connection for {}", self.address),
                    status,
                ));
            }
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn disconnect_best_effort(&mut self) {
        if let Err(error) = self.disconnect() {
            log::debug!("failed to clean up macOS RFCOMM state: {error:#}");
        }
    }

    fn release_completed_sdp_delegate(&mut self) {
        let completed = self
            .sdp_status
            .as_ref()
            .is_none_or(|status| status.load(Ordering::Acquire) != CALLBACK_PENDING);
        if completed {
            self.sdp_delegate.take();
            self.sdp_status.take();
        }
    }

    fn preserve_pending_sdp_delegate(&mut self) {
        let pending = self
            .sdp_status
            .as_ref()
            .is_some_and(|status| status.load(Ordering::Acquire) == CALLBACK_PENDING);
        if pending && let Some(delegate) = self.sdp_delegate.take() {
            std::mem::forget(delegate);
        }
        self.sdp_status.take();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum HelperOpcode {
    Initialize = 1,
    Connect = 2,
    IsConnected = 3,
    Write = 4,
    Read = 5,
    Disconnect = 6,
    Shutdown = 7,
}

impl TryFrom<u8> for HelperOpcode {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Initialize),
            2 => Ok(Self::Connect),
            3 => Ok(Self::IsConnected),
            4 => Ok(Self::Write),
            5 => Ok(Self::Read),
            6 => Ok(Self::Disconnect),
            7 => Ok(Self::Shutdown),
            _ => bail!("unknown macOS RFCOMM helper opcode {value}"),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum HelperRequest {
    Initialize {
        address: String,
        channel: Option<u8>,
    },
    Connect,
    IsConnected,
    Write(Vec<u8>),
    Read(Duration),
    Disconnect,
    Shutdown,
}

impl HelperRequest {
    fn opcode(&self) -> HelperOpcode {
        match self {
            Self::Initialize { .. } => HelperOpcode::Initialize,
            Self::Connect => HelperOpcode::Connect,
            Self::IsConnected => HelperOpcode::IsConnected,
            Self::Write(_) => HelperOpcode::Write,
            Self::Read(_) => HelperOpcode::Read,
            Self::Disconnect => HelperOpcode::Disconnect,
            Self::Shutdown => HelperOpcode::Shutdown,
        }
    }
}

enum HelperInput {
    Request(HelperRequest),
    ProtocolError { opcode: u8, message: String },
}

pub(crate) struct MacRfcommBackend {
    address: String,
    channel: Option<u8>,
    helper: Mutex<HelperProcess>,
    connected: AtomicBool,
}

impl MacRfcommBackend {
    pub(crate) fn new(address: String, channel: Option<u8>) -> Result<Self> {
        let address = normalize_address(&address)?;
        if let Some(channel) = channel {
            validate_channel(channel)?;
        }

        let helper = spawn_initialized_helper(&address, channel)?;

        Ok(Self {
            address,
            channel,
            helper: Mutex::new(helper),
            connected: AtomicBool::new(false),
        })
    }

    pub(crate) fn connect(&mut self) -> Result<()> {
        let result = self
            .connect_with_supervision()
            .with_context(|| format!("failed to connect RFCOMM device {}", self.address));
        self.connected.store(result.is_ok(), Ordering::Release);
        result.map(|_| ())
    }

    pub(crate) fn is_connected(&self) -> bool {
        let result = self
            .request(HelperRequest::IsConnected, IS_CONNECTED_REPLY_TIMEOUT)
            .and_then(|body| match body.as_slice() {
                [0] => Ok(false),
                [1] => Ok(true),
                _ => bail!("invalid is-connected response from the macOS RFCOMM helper"),
            });
        match result {
            Ok(connected) => {
                self.connected.store(connected, Ordering::Relaxed);
                connected
            }
            Err(error) => {
                log::debug!(
                    "failed to query macOS RFCOMM helper connection state for {}: {error:#}",
                    self.address
                );
                self.connected.store(false, Ordering::Relaxed);
                false
            }
        }
    }

    pub(crate) fn write_all(&mut self, data: &[u8]) -> Result<()> {
        for chunk in data.chunks(MAX_HELPER_WRITE_PAYLOAD) {
            if let Err(error) =
                self.request(HelperRequest::Write(chunk.to_vec()), WRITE_REPLY_TIMEOUT)
            {
                self.connected.store(false, Ordering::Relaxed);
                return Err(error)
                    .with_context(|| format!("failed to write to RFCOMM device {}", self.address));
            }
        }
        Ok(())
    }

    pub(crate) fn read(&mut self, timeout: Duration) -> Result<Vec<u8>> {
        let timeout_millis = helper_timeout_millis(timeout)?;
        let timeout = Duration::from_millis(timeout_millis);
        self.request(
            HelperRequest::Read(timeout),
            timeout.saturating_add(HELPER_REPLY_GRACE),
        )
        .with_context(|| format!("failed to read from RFCOMM device {}", self.address))
    }

    pub(crate) fn disconnect(&mut self) -> Result<()> {
        self.connected.store(false, Ordering::Relaxed);
        let mut helper = self
            .helper
            .lock()
            .map_err(|_| anyhow!("macOS RFCOMM helper lock was poisoned"))?;

        if !helper.is_usable() {
            return Ok(());
        }

        let result = helper.request(HelperRequest::Disconnect, DISCONNECT_REPLY_TIMEOUT);
        if let Err(error) = result {
            let helper_pipe_closed = error.chain().any(|cause| {
                cause
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
            });
            if helper_pipe_closed {
                log::debug!(
                    "macOS RFCOMM helper pipe closed while disconnecting {}; treating the device as disconnected: {error:#}",
                    self.address
                );
                return Ok(());
            }
            return Err(error)
                .with_context(|| format!("failed to disconnect RFCOMM device {}", self.address));
        }

        Ok(())
    }

    fn request(&self, request: HelperRequest, timeout: Duration) -> Result<Vec<u8>> {
        if !self.connected.load(Ordering::Relaxed) {
            bail!("RFCOMM device already disconnected");
        }

        self.helper
            .lock()
            .map_err(|_| anyhow!("macOS RFCOMM helper lock was poisoned"))?
            .request(request, timeout)
    }

    fn connect_with_supervision(&self) -> Result<Vec<u8>> {
        let mut helper = self
            .helper
            .lock()
            .map_err(|_| anyhow!("macOS RFCOMM helper lock was poisoned"))?;

        if !helper.is_usable() {
            *helper = spawn_initialized_helper(&self.address, self.channel)
                .context("failed to restart the macOS RFCOMM helper before connecting")?;
        }

        match helper.request(HelperRequest::Connect, CONNECT_REPLY_TIMEOUT) {
            Ok(response) => Ok(response),
            Err(first_error) => {
                let helper_usable = helper.is_usable();
                if !should_retry_connect(true, helper_usable) {
                    return Err(first_error);
                }

                log::debug!(
                    "restarting the macOS RFCOMM helper after it failed while connecting to {}: {first_error:#}",
                    self.address
                );
                *helper = spawn_initialized_helper(&self.address, self.channel).with_context(|| {
                    format!(
                        "failed to restart the macOS RFCOMM helper after a connect attempt failed: {first_error:#}"
                    )
                })?;
                helper.request(HelperRequest::Connect, CONNECT_REPLY_TIMEOUT)
            }
        }
    }
}

fn spawn_initialized_helper(address: &str, channel: Option<u8>) -> Result<HelperProcess> {
    let mut helper = HelperProcess::spawn()?;
    helper
        .request(
            HelperRequest::Initialize {
                address: address.to_owned(),
                channel,
            },
            HELPER_START_TIMEOUT,
        )
        .context("failed to initialize the macOS RFCOMM helper")?;
    Ok(helper)
}

fn should_retry_connect(request_failed: bool, helper_usable: bool) -> bool {
    request_failed && !helper_usable
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HelperChildState {
    Missing,
    Running,
    Exited,
    Uninspectable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HelperResponseReaderState {
    Missing,
    Running,
    Finished,
}

fn helper_state_is_usable(
    child_state: HelperChildState,
    input_available: bool,
    response_reader_state: HelperResponseReaderState,
) -> bool {
    child_state == HelperChildState::Running
        && input_available
        && response_reader_state == HelperResponseReaderState::Running
}

struct HelperProcess {
    child: Option<Child>,
    input: Option<BufWriter<ChildStdin>>,
    responses: mpsc::Receiver<std::result::Result<Vec<u8>, String>>,
    response_reader: Option<JoinHandle<()>>,
}

impl HelperProcess {
    fn spawn() -> Result<Self> {
        let executable = env::current_exe().context("failed to locate the current executable")?;
        let mut child = Command::new(&executable)
            // run helper
            .env(HELPER_MODE_ENV, HELPER_MODE_VALUE)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| {
                format!(
                    "failed to start macOS RFCOMM helper {}",
                    executable.display()
                )
            })?;
        let input = match child.stdin.take() {
            Some(input) => input,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("macOS RFCOMM helper did not expose stdin");
            }
        };
        let output = match child.stdout.take() {
            Some(output) => output,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("macOS RFCOMM helper did not expose stdout");
            }
        };

        let (response_tx, responses) = mpsc::sync_channel(1);
        let response_reader = match thread::Builder::new()
            .name("luck-jingle-helper-responses".to_owned())
            .spawn(move || helper_response_reader(output, response_tx))
        {
            Ok(reader) => reader,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("failed to start the RFCOMM helper response reader");
            }
        };

        Ok(Self {
            child: Some(child),
            input: Some(BufWriter::new(input)),
            responses,
            response_reader: Some(response_reader),
        })
    }

    fn is_usable(&mut self) -> bool {
        let child_state = match self.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(None) => HelperChildState::Running,
                Ok(Some(_)) => HelperChildState::Exited,
                Err(error) => {
                    log::debug!("failed to inspect the macOS RFCOMM helper: {error}");
                    HelperChildState::Uninspectable
                }
            },
            None => HelperChildState::Missing,
        };
        let response_reader_state = match self.response_reader.as_ref() {
            Some(reader) if reader.is_finished() => HelperResponseReaderState::Finished,
            Some(_) => HelperResponseReaderState::Running,
            None => HelperResponseReaderState::Missing,
        };
        let usable =
            helper_state_is_usable(child_state, self.input.is_some(), response_reader_state);
        if !usable {
            self.terminate();
        }
        usable
    }

    fn request(&mut self, request: HelperRequest, timeout: Duration) -> Result<Vec<u8>> {
        let opcode = request.opcode();
        let payload = encode_helper_request(&request)?;
        let write_result = self
            .input
            .as_mut()
            .context("macOS RFCOMM helper stdin is closed")
            .and_then(|input| {
                write_frame(input, &payload, MAX_HELPER_REQUEST_PAYLOAD)
                    .context("failed to write a request to the macOS RFCOMM helper")
            });
        if let Err(error) = write_result {
            self.terminate();
            return Err(error);
        }

        let response = match self.responses.recv_timeout(timeout) {
            Ok(Ok(response)) => response,
            Ok(Err(message)) => {
                self.terminate();
                bail!("{message}");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.terminate();
                bail!("timed out waiting for the macOS RFCOMM helper");
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.terminate();
                bail!("macOS RFCOMM helper stopped before replying");
            }
        };
        decode_helper_response(&response, opcode)
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> bool {
        let deadline = deadline_after(timeout);
        loop {
            let status = match self.child.as_mut() {
                Some(child) => child.try_wait(),
                None => return true,
            };
            match status {
                Ok(Some(_)) => {
                    self.child.take();
                    self.join_response_reader();
                    return true;
                }
                Ok(None) => {}
                Err(_) => return false,
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn terminate(&mut self) {
        self.input.take();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.join_response_reader();
    }

    fn join_response_reader(&mut self) {
        if let Some(reader) = self.response_reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for HelperProcess {
    fn drop(&mut self) {
        if self.child.is_none() {
            self.join_response_reader();
            return;
        }

        let shutdown_succeeded = self
            .request(HelperRequest::Shutdown, DROP_REPLY_TIMEOUT)
            .is_ok();
        drop(self.input.take());
        if !shutdown_succeeded || !self.wait_for_exit(HELPER_EXIT_GRACE) {
            self.terminate();
        }
    }
}

pub fn run_helper_if_requested() -> Option<Result<()>> {
    let requested =
        env::var_os(HELPER_MODE_ENV).is_some_and(|value| value == OsStr::new(HELPER_MODE_VALUE));
    requested.then(run_helper)
}

fn run_helper() -> Result<()> {
    if !NSThread::isMainThread_class() {
        bail!("macOS RFCOMM helper must run on the process main thread");
    }

    let (request_tx, requests) = mpsc::sync_channel(1);
    let _request_reader = thread::Builder::new()
        .name("luck-jingle-helper-requests".to_owned())
        .spawn(move || helper_request_reader(request_tx))
        .context("failed to start the RFCOMM helper request reader")?;
    let stdout = io::stdout();
    let mut output = BufWriter::new(stdout.lock());
    let run_loop = NSRunLoop::currentRunLoop();
    let mut context = None;

    loop {
        match requests.recv_timeout(RUN_LOOP_SLICE) {
            Ok(HelperInput::Request(request)) => {
                let opcode = request.opcode() as u8;
                let (result, shutdown) = execute_helper_request(request, &mut context);
                let response = encode_helper_response(opcode, result);
                write_frame(&mut output, &response, MAX_HELPER_RESPONSE_PAYLOAD)
                    .context("failed to write a response from the macOS RFCOMM helper")?;
                if shutdown {
                    break;
                }
            }
            Ok(HelperInput::ProtocolError { opcode, message }) => {
                let response = encode_helper_response(opcode, Err(anyhow!(message)));
                write_frame(&mut output, &response, MAX_HELPER_RESPONSE_PAYLOAD)
                    .context("failed to report a macOS RFCOMM helper protocol error")?;
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                autoreleasepool(|_| pump_run_loop(&run_loop, RUN_LOOP_SLICE));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    cleanup_helper_context(&mut context);
    Ok(())
}

fn execute_helper_request(
    request: HelperRequest,
    context: &mut Option<WorkerContext>,
) -> (Result<Vec<u8>>, bool) {
    let shutdown = matches!(&request, HelperRequest::Shutdown);
    let result = match request {
        HelperRequest::Initialize { address, channel } => {
            if context.is_some() {
                Err(anyhow!("macOS RFCOMM helper is already initialized"))
            } else {
                let read_state = Arc::new(SharedReadState::default());
                read_state.mark_disconnected(format!("RFCOMM device {address} is not connected"));
                *context = Some(autoreleasepool(|_| {
                    WorkerContext::new(address, channel, read_state)
                }));
                Ok(Vec::new())
            }
        }
        HelperRequest::Connect => helper_context_mut(context).and_then(|context| {
            let result = autoreleasepool(|_| context.connect());
            // The framework may still reference this unretained target while
            // autoreleased SDP objects drain.
            context.release_completed_sdp_delegate();
            result.map(|_| Vec::new())
        }),
        HelperRequest::IsConnected => {
            helper_context_mut(context).map(|context| vec![u8::from(context.is_connected())])
        }
        HelperRequest::Write(data) => helper_context_mut(context).and_then(|context| {
            let result = autoreleasepool(|_| context.write_all(&data));
            if result.is_err() {
                autoreleasepool(|_| context.disconnect_best_effort());
            }
            result.map(|_| Vec::new())
        }),
        HelperRequest::Read(timeout) => {
            helper_context_mut(context).and_then(|context| context.read(timeout))
        }
        HelperRequest::Disconnect => helper_context_mut(context)
            .and_then(|context| autoreleasepool(|_| context.disconnect()))
            .map(|_| Vec::new()),
        HelperRequest::Shutdown => {
            cleanup_helper_context(context);
            Ok(Vec::new())
        }
    };
    (result, shutdown)
}

fn helper_context_mut(context: &mut Option<WorkerContext>) -> Result<&mut WorkerContext> {
    context
        .as_mut()
        .context("macOS RFCOMM helper is not initialized")
}

fn cleanup_helper_context(context: &mut Option<WorkerContext>) {
    if let Some(mut context) = context.take() {
        autoreleasepool(|_| context.disconnect_best_effort());
        context.release_completed_sdp_delegate();
        context.preserve_pending_sdp_delegate();
    }
}

fn helper_request_reader(sender: mpsc::SyncSender<HelperInput>) {
    let stdin = io::stdin();
    let mut input = BufReader::new(stdin.lock());
    loop {
        let payload = match read_frame(&mut input, MAX_HELPER_REQUEST_PAYLOAD) {
            Ok(Some(payload)) => payload,
            Ok(None) => return,
            Err(error) => {
                let _ = sender.send(HelperInput::ProtocolError {
                    opcode: 0,
                    message: format!("invalid macOS RFCOMM helper request frame: {error:#}"),
                });
                return;
            }
        };
        let opcode = payload.first().copied().unwrap_or(0);
        match decode_helper_request(&payload) {
            Ok(request) => {
                if sender.send(HelperInput::Request(request)).is_err() {
                    return;
                }
            }
            Err(error) => {
                let _ = sender.send(HelperInput::ProtocolError {
                    opcode,
                    message: format!("invalid macOS RFCOMM helper request: {error:#}"),
                });
                return;
            }
        }
    }
}

fn helper_response_reader(
    output: std::process::ChildStdout,
    sender: mpsc::SyncSender<std::result::Result<Vec<u8>, String>>,
) {
    let mut output = BufReader::new(output);
    loop {
        match read_frame(&mut output, MAX_HELPER_RESPONSE_PAYLOAD) {
            Ok(Some(payload)) => {
                if sender.send(Ok(payload)).is_err() {
                    return;
                }
            }
            Ok(None) => {
                let _ = sender.send(Err(
                    "macOS RFCOMM helper closed stdout before replying".to_owned()
                ));
                return;
            }
            Err(error) => {
                let _ = sender.send(Err(format!(
                    "invalid response frame from the macOS RFCOMM helper: {error:#}"
                )));
                return;
            }
        }
    }
}

fn encode_helper_request(request: &HelperRequest) -> Result<Vec<u8>> {
    let mut payload = vec![request.opcode() as u8];
    match request {
        HelperRequest::Initialize { address, channel } => {
            let address = normalize_address(address)?;
            if let Some(channel) = channel {
                validate_channel(*channel)?;
            }
            payload.push(channel.unwrap_or(0));
            payload.extend_from_slice(address.as_bytes());
        }
        HelperRequest::Write(data) => {
            if data.len() > MAX_HELPER_WRITE_PAYLOAD {
                bail!(
                    "macOS RFCOMM helper write payload exceeds the {} byte limit",
                    MAX_HELPER_WRITE_PAYLOAD
                );
            }
            payload.extend_from_slice(data);
        }
        HelperRequest::Read(timeout) => {
            payload.extend_from_slice(&helper_timeout_millis(*timeout)?.to_be_bytes());
        }
        HelperRequest::Connect
        | HelperRequest::IsConnected
        | HelperRequest::Disconnect
        | HelperRequest::Shutdown => {}
    }
    if payload.len() > MAX_HELPER_REQUEST_PAYLOAD {
        bail!("macOS RFCOMM helper request exceeds the frame limit");
    }
    Ok(payload)
}

fn decode_helper_request(payload: &[u8]) -> Result<HelperRequest> {
    let (&opcode, body) = payload
        .split_first()
        .context("macOS RFCOMM helper request is empty")?;
    let opcode = HelperOpcode::try_from(opcode)?;
    match opcode {
        HelperOpcode::Initialize => {
            if body.len() != 18 {
                bail!("invalid initialize request length {}", payload.len());
            }
            let channel = match body[0] {
                0 => None,
                channel => {
                    validate_channel(channel)?;
                    Some(channel)
                }
            };
            let address = std::str::from_utf8(&body[1..])
                .context("initialize request contains a non-UTF-8 Bluetooth address")?;
            Ok(HelperRequest::Initialize {
                address: normalize_address(address)?,
                channel,
            })
        }
        HelperOpcode::Connect => {
            require_empty_helper_body(opcode, body)?;
            Ok(HelperRequest::Connect)
        }
        HelperOpcode::IsConnected => {
            require_empty_helper_body(opcode, body)?;
            Ok(HelperRequest::IsConnected)
        }
        HelperOpcode::Write => {
            if body.len() > MAX_HELPER_WRITE_PAYLOAD {
                bail!(
                    "macOS RFCOMM helper write payload exceeds the {} byte limit",
                    MAX_HELPER_WRITE_PAYLOAD
                );
            }
            Ok(HelperRequest::Write(body.to_vec()))
        }
        HelperOpcode::Read => {
            let bytes: [u8; 8] = body
                .try_into()
                .map_err(|_| anyhow!("invalid read request length {}", payload.len()))?;
            let timeout = Duration::from_millis(u64::from_be_bytes(bytes));
            helper_timeout_millis(timeout)?;
            Ok(HelperRequest::Read(timeout))
        }
        HelperOpcode::Disconnect => {
            require_empty_helper_body(opcode, body)?;
            Ok(HelperRequest::Disconnect)
        }
        HelperOpcode::Shutdown => {
            require_empty_helper_body(opcode, body)?;
            Ok(HelperRequest::Shutdown)
        }
    }
}

fn require_empty_helper_body(opcode: HelperOpcode, body: &[u8]) -> Result<()> {
    if !body.is_empty() {
        bail!("macOS RFCOMM helper {opcode:?} request must not contain a body");
    }
    Ok(())
}

fn encode_helper_response(opcode: u8, result: Result<Vec<u8>>) -> Vec<u8> {
    match result {
        Ok(body) => {
            let mut payload = Vec::with_capacity(2 + body.len());
            payload.extend_from_slice(&[opcode, 0]);
            payload.extend_from_slice(&body);
            payload
        }
        Err(error) => {
            let formatted = format!("{error:#}");
            let message = bounded_error_message(&formatted);
            let mut payload = Vec::with_capacity(2 + message.len());
            payload.extend_from_slice(&[opcode, 1]);
            payload.extend_from_slice(message.as_bytes());
            payload
        }
    }
}

fn decode_helper_response(payload: &[u8], expected_opcode: HelperOpcode) -> Result<Vec<u8>> {
    if payload.len() < 2 {
        bail!("macOS RFCOMM helper response is too short");
    }
    let opcode = payload[0];
    let status = payload[1];
    let body = &payload[2..];
    if opcode != expected_opcode as u8 {
        bail!(
            "macOS RFCOMM helper response opcode mismatch: expected {}, got {opcode}",
            expected_opcode as u8
        );
    }
    if status == 1 {
        let message =
            std::str::from_utf8(body).context("macOS RFCOMM helper returned a non-UTF-8 error")?;
        bail!(
            "macOS RFCOMM helper failed: {}",
            if message.is_empty() {
                "unspecified helper error"
            } else {
                message
            }
        );
    }
    if status != 0 {
        bail!("macOS RFCOMM helper returned invalid response status {status}");
    }
    match expected_opcode {
        HelperOpcode::IsConnected if body.len() != 1 => {
            bail!("invalid is-connected response length {}", body.len());
        }
        HelperOpcode::Read if body.len() > READ_BUFFER_SIZE => {
            bail!("invalid read response length {}", body.len());
        }
        HelperOpcode::Initialize
        | HelperOpcode::Connect
        | HelperOpcode::Write
        | HelperOpcode::Disconnect
        | HelperOpcode::Shutdown
            if !body.is_empty() =>
        {
            bail!("macOS RFCOMM helper {expected_opcode:?} response must not contain a body");
        }
        _ => {}
    }
    Ok(body.to_vec())
}

fn bounded_error_message(message: &str) -> &str {
    if message.len() <= MAX_HELPER_ERROR_BYTES {
        return message;
    }
    let mut end = MAX_HELPER_ERROR_BYTES;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    &message[..end]
}

fn helper_timeout_millis(timeout: Duration) -> Result<u64> {
    if timeout > MAX_HELPER_READ_TIMEOUT {
        bail!(
            "macOS RFCOMM helper read timeout exceeds the {} second limit",
            MAX_HELPER_READ_TIMEOUT.as_secs()
        );
    }
    let mut millis = timeout.as_millis() as u64;
    if Duration::from_millis(millis) < timeout {
        millis = millis.saturating_add(1);
    }
    Ok(millis)
}

fn write_frame<W: Write>(writer: &mut W, payload: &[u8], maximum: usize) -> Result<()> {
    if payload.is_empty() {
        bail!("macOS RFCOMM helper frame payload must not be empty");
    }
    if payload.len() > maximum || payload.len() > u32::MAX as usize {
        bail!(
            "macOS RFCOMM helper frame payload length {} exceeds the {maximum} byte limit",
            payload.len()
        );
    }
    writer
        .write_all(&(payload.len() as u32).to_be_bytes())
        .context("failed to write the macOS RFCOMM helper frame length")?;
    writer
        .write_all(payload)
        .context("failed to write the macOS RFCOMM helper frame payload")?;
    writer
        .flush()
        .context("failed to flush the macOS RFCOMM helper frame")?;
    Ok(())
}

fn read_frame<R: Read>(reader: &mut R, maximum: usize) -> Result<Option<Vec<u8>>> {
    let mut length = [0_u8; 4];
    loop {
        match reader.read(&mut length[..1]) {
            Ok(0) => return Ok(None),
            Ok(1) => break,
            Ok(_) => unreachable!(),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("failed to read helper frame length"),
        }
    }
    reader
        .read_exact(&mut length[1..])
        .context("truncated macOS RFCOMM helper frame length")?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 {
        bail!("macOS RFCOMM helper frame payload must not be empty");
    }
    if length > maximum {
        bail!("macOS RFCOMM helper frame payload length {length} exceeds the {maximum} byte limit");
    }
    let mut payload = vec![0_u8; length];
    reader
        .read_exact(&mut payload)
        .context("truncated macOS RFCOMM helper frame payload")?;
    Ok(Some(payload))
}

fn pump_run_loop(run_loop: &NSRunLoop, duration: Duration) {
    if duration.is_zero() {
        return;
    }
    let limit = NSDate::dateWithTimeIntervalSinceNow(duration.as_secs_f64());
    unsafe {
        let _ = run_loop.runMode_beforeDate(NSDefaultRunLoopMode, &limit);
    }
}

fn normalize_address(address: &str) -> Result<String> {
    let address = address.trim();
    if !address.is_ascii() {
        bail!("invalid Bluetooth address: {address}");
    }
    let parts: Vec<&str> = if address.contains(':') {
        address.split(':').collect()
    } else if address.contains('-') {
        address.split('-').collect()
    } else if address.len() == 12 {
        (0..6)
            .map(|index| &address[index * 2..index * 2 + 2])
            .collect()
    } else {
        Vec::new()
    };

    if parts.len() != 6
        || parts
            .iter()
            .any(|part| part.len() != 2 || !part.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        bail!("invalid Bluetooth address: {address}");
    }

    Ok(parts
        .into_iter()
        .map(str::to_ascii_uppercase)
        .collect::<Vec<_>>()
        .join(":"))
}

fn device_identity(device: &IOBluetoothDevice) -> Option<(String, Option<String>)> {
    let address = unsafe { device.addressString() }?;
    let address = normalize_address(&address.to_string()).ok()?;
    Some((address, cached_device_name(device)))
}

#[allow(deprecated)]
fn cached_device_name(device: &IOBluetoothDevice) -> Option<String> {
    unsafe { device.getName() }.and_then(|name| normalize_device_name(name.to_string()))
}

fn resolve_device_name(
    device: &IOBluetoothDevice,
    address: &str,
    observed_name: Option<String>,
    page_timeout: Option<BluetoothHCIPageTimeout>,
) -> Option<String> {
    let name = cached_device_name(device).or(observed_name);
    if name.is_some() {
        return name;
    }
    let Some(page_timeout) = page_timeout else {
        log::debug!(
            "skipping remote Bluetooth name request for {address} because the name-resolution budget was exhausted"
        );
        return None;
    };

    log::debug!("requesting missing Bluetooth name for {address}");
    let status = unsafe { device.remoteNameRequest_withPageTimeout(None, page_timeout) };
    if status != 0 {
        log::debug!(
            "remote Bluetooth name request failed for {address} with IOReturn 0x{:08X}",
            status as u32
        );
        return None;
    }

    let name = cached_device_name(device);
    if name.is_none() {
        log::debug!(
            "remote Bluetooth name request succeeded for {address} without returning a name"
        );
    }
    name
}

fn normalize_device_name(name: String) -> Option<String> {
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

fn latest_name(current: Option<String>, candidate: Option<String>) -> Option<String> {
    match (current, candidate) {
        (_, Some(candidate)) => Some(candidate),
        (Some(current), None) => Some(current),
        (None, None) => None,
    }
}

fn validate_channel(channel: u8) -> Result<()> {
    if !(1..=30).contains(&channel) {
        bail!("invalid RFCOMM channel {channel}; expected 1..=30");
    }
    Ok(())
}

fn inquiry_seconds(timeout: Duration) -> u8 {
    let mut seconds = timeout.as_secs();
    if timeout.subsec_nanos() != 0 {
        seconds = seconds.saturating_add(1);
    }
    seconds.clamp(1, MAX_INQUIRY_SECONDS) as u8
}

fn remote_name_page_timeout(remaining: Duration) -> Option<BluetoothHCIPageTimeout> {
    let slots = remaining.as_micros() / HCI_SLOT_MICROS;
    if slots == 0 {
        return None;
    }
    Some(slots.min(REMOTE_NAME_MAX_PAGE_TIMEOUT as u128) as BluetoothHCIPageTimeout)
}

fn effective_write_chunk(mtu: usize) -> Result<usize> {
    if mtu == 0 {
        bail!("macOS reported an RFCOMM MTU of zero");
    }
    Ok(mtu.min(MAX_WRITE_CHUNK).min(u16::MAX as usize))
}

fn normalize_discovered_devices(
    devices: impl IntoIterator<Item = DiscoveredDevice>,
) -> Vec<DiscoveredDevice> {
    let mut by_address = BTreeMap::new();
    for device in devices {
        by_address
            .entry(device.address.clone())
            .and_modify(|current: &mut DiscoveredDevice| {
                current.name = latest_name(current.name.take(), device.name.clone());
            })
            .or_insert(device);
    }
    let mut devices: Vec<_> = by_address.into_values().collect();
    devices.sort_by(|left, right| {
        left.name
            .is_none()
            .cmp(&right.name.is_none())
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.address.cmp(&right.address))
    });
    devices
}

fn deadline_after(duration: Duration) -> Instant {
    Instant::now()
        .checked_add(duration)
        .unwrap_or_else(Instant::now)
}

fn io_return_error(operation: &str, status: i32) -> anyhow::Error {
    anyhow!("{operation} (IOReturn 0x{:08X})", status as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_normalization_accepts_apple_and_standard_formats() {
        assert_eq!(
            normalize_address("aa-bb-cc-dd-ee-ff").unwrap(),
            "AA:BB:CC:DD:EE:FF"
        );
        assert_eq!(
            normalize_address("AA:BB:CC:DD:EE:FF").unwrap(),
            "AA:BB:CC:DD:EE:FF"
        );
        assert_eq!(
            normalize_address("aabbccddeeff").unwrap(),
            "AA:BB:CC:DD:EE:FF"
        );
    }

    #[test]
    fn address_normalization_rejects_invalid_values() {
        assert!(normalize_address("AA:BB:CC:DD:EE").is_err());
        assert!(normalize_address("AA:BB:CC:DD:EE:GG").is_err());
        assert!(normalize_address("AA-BB:CC-DD-EE-FF").is_err());
    }

    #[test]
    fn inquiry_duration_rounds_up_and_clamps_to_controller_limit() {
        assert_eq!(inquiry_seconds(Duration::from_nanos(1)), 1);
        assert_eq!(inquiry_seconds(Duration::from_millis(1_001)), 2);
        assert_eq!(inquiry_seconds(Duration::from_secs(90)), 48);
    }

    #[test]
    fn remote_name_page_timeout_is_bounded_by_the_remaining_budget() {
        assert_eq!(
            remote_name_page_timeout(Duration::from_secs(10)),
            Some(REMOTE_NAME_MAX_PAGE_TIMEOUT)
        );
        assert_eq!(
            remote_name_page_timeout(Duration::from_millis(1_250)),
            Some(2_000)
        );
        assert_eq!(remote_name_page_timeout(Duration::from_micros(624)), None);
    }

    #[test]
    fn write_chunk_respects_mtu_and_global_limit() {
        assert_eq!(effective_write_chunk(512).unwrap(), 512);
        assert_eq!(effective_write_chunk(usize::MAX).unwrap(), MAX_WRITE_CHUNK);
        assert!(effective_write_chunk(0).is_err());
    }

    #[test]
    fn discovered_devices_are_deduplicated_and_sorted() {
        let devices = normalize_discovered_devices([
            DiscoveredDevice {
                name: Some("Printer B".to_owned()),
                address: "AA:BB:CC:DD:EE:02".to_owned(),
            },
            DiscoveredDevice {
                name: None,
                address: "AA:BB:CC:DD:EE:01".to_owned(),
            },
            DiscoveredDevice {
                name: Some("Printer A".to_owned()),
                address: "AA:BB:CC:DD:EE:01".to_owned(),
            },
            DiscoveredDevice {
                name: None,
                address: "AA:BB:CC:DD:EE:03".to_owned(),
            },
        ]);

        assert_eq!(devices.len(), 3);
        assert_eq!(devices[0].name.as_deref(), Some("Printer A"));
        assert_eq!(devices[1].name.as_deref(), Some("Printer B"));
        assert_eq!(devices[2].name, None);
        assert_eq!(devices[2].address, "AA:BB:CC:DD:EE:03");
    }

    #[test]
    fn device_names_are_trimmed_and_new_observations_replace_stale_names() {
        assert_eq!(
            normalize_device_name("  LuckP_D1X_Test  ".to_owned()).as_deref(),
            Some("LuckP_D1X_Test")
        );
        assert_eq!(normalize_device_name("  ".to_owned()), None);
        assert_eq!(
            latest_name(
                Some("Old Name".to_owned()),
                Some("LuckP_D1X_Test".to_owned())
            )
            .as_deref(),
            Some("LuckP_D1X_Test")
        );
    }

    #[test]
    fn receive_buffer_returns_data_and_reports_disconnect() {
        let state = SharedReadState::default();
        state.mark_connected();
        state.append(&[1, 2, 3]);
        assert_eq!(state.take_available().unwrap(), Some(vec![1, 2, 3]));
        assert_eq!(state.take_available().unwrap(), None);

        state.mark_disconnected("test disconnect");
        let error = state.take_available().unwrap_err();
        assert!(error.to_string().contains("test disconnect"));
    }

    #[test]
    fn helper_requests_round_trip_through_the_binary_protocol() {
        let requests = [
            HelperRequest::Initialize {
                address: "AA:BB:CC:DD:EE:FF".to_owned(),
                channel: Some(7),
            },
            HelperRequest::Connect,
            HelperRequest::IsConnected,
            HelperRequest::Write(vec![0, 1, 2, 255]),
            HelperRequest::Read(Duration::from_millis(1_234)),
            HelperRequest::Disconnect,
            HelperRequest::Shutdown,
        ];

        for request in requests {
            let encoded = encode_helper_request(&request).unwrap();
            assert_eq!(decode_helper_request(&encoded).unwrap(), request);
        }
    }

    #[test]
    fn helper_frames_reject_empty_truncated_and_oversized_payloads() {
        let mut empty = std::io::Cursor::new(0_u32.to_be_bytes());
        assert!(
            read_frame(&mut empty, 16)
                .unwrap_err()
                .to_string()
                .contains("empty")
        );

        let mut truncated = std::io::Cursor::new([0, 0, 0, 2, 1]);
        assert!(
            read_frame(&mut truncated, 16)
                .unwrap_err()
                .to_string()
                .contains("truncated")
        );

        let oversized_length = (MAX_HELPER_REQUEST_PAYLOAD as u32 + 1).to_be_bytes();
        let mut oversized = std::io::Cursor::new(oversized_length);
        assert!(
            read_frame(&mut oversized, MAX_HELPER_REQUEST_PAYLOAD)
                .unwrap_err()
                .to_string()
                .contains("exceeds")
        );
    }

    #[test]
    fn helper_write_and_read_budgets_are_enforced() {
        let maximum_write = HelperRequest::Write(vec![0; MAX_HELPER_WRITE_PAYLOAD]);
        assert_eq!(
            encode_helper_request(&maximum_write).unwrap().len(),
            MAX_HELPER_REQUEST_PAYLOAD
        );
        let oversized_write = HelperRequest::Write(vec![0; MAX_HELPER_WRITE_PAYLOAD + 1]);
        assert!(encode_helper_request(&oversized_write).is_err());

        assert_eq!(helper_timeout_millis(Duration::from_nanos(1)).unwrap(), 1);
        assert_eq!(
            helper_timeout_millis(MAX_HELPER_READ_TIMEOUT).unwrap(),
            MAX_HELPER_READ_TIMEOUT.as_millis() as u64
        );
        assert!(helper_timeout_millis(MAX_HELPER_READ_TIMEOUT + Duration::from_millis(1)).is_err());
    }

    #[test]
    fn helper_responses_validate_status_opcode_and_error_budget() {
        let success = encode_helper_response(HelperOpcode::Read as u8, Ok(vec![1, 2, 3]));
        assert_eq!(
            decode_helper_response(&success, HelperOpcode::Read).unwrap(),
            vec![1, 2, 3]
        );
        assert!(decode_helper_response(&success, HelperOpcode::Write).is_err());
        let unexpected_body = encode_helper_response(HelperOpcode::Connect as u8, Ok(vec![1]));
        assert!(decode_helper_response(&unexpected_body, HelperOpcode::Connect).is_err());

        let long_error = "error".repeat(MAX_HELPER_ERROR_BYTES);
        let failure = encode_helper_response(HelperOpcode::Connect as u8, Err(anyhow!(long_error)));
        assert_eq!(failure.len(), 2 + MAX_HELPER_ERROR_BYTES);
        assert!(decode_helper_response(&failure, HelperOpcode::Connect).is_err());
    }

    #[test]
    fn backend_retains_explicit_and_discovered_channels() {
        for channel in [None, Some(7)] {
            let (_response_tx, responses) = mpsc::sync_channel(1);
            let backend = MacRfcommBackend {
                address: "AA:BB:CC:DD:EE:FF".to_owned(),
                channel,
                helper: Mutex::new(HelperProcess {
                    child: None,
                    input: None,
                    responses,
                    response_reader: None,
                }),
                connected: AtomicBool::new(false),
            };

            assert_eq!(backend.channel, channel);
        }
    }

    #[test]
    fn disconnect_succeeds_when_the_helper_is_already_unavailable() {
        let (_response_tx, responses) = mpsc::sync_channel(1);
        let mut backend = MacRfcommBackend {
            address: "AA:BB:CC:DD:EE:FF".to_owned(),
            channel: None,
            helper: Mutex::new(HelperProcess {
                child: None,
                input: None,
                responses,
                response_reader: None,
            }),
            connected: AtomicBool::new(true),
        };

        backend.disconnect().unwrap();

        assert!(!backend.connected.load(Ordering::Acquire));
    }

    #[test]
    fn disconnect_succeeds_when_the_helper_pipe_breaks_after_the_usability_check() {
        let mut closed_input_child = Command::new("/usr/bin/true")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let closed_input = closed_input_child.stdin.take().unwrap();
        closed_input_child.wait().unwrap();

        let mut live_child = Command::new("/bin/sleep")
            .arg("30")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let live_output = live_child.stdout.take().unwrap();
        let (response_tx, responses) = mpsc::sync_channel(1);
        let response_reader =
            thread::spawn(move || helper_response_reader(live_output, response_tx));
        let mut backend = MacRfcommBackend {
            address: "AA:BB:CC:DD:EE:FF".to_owned(),
            channel: None,
            helper: Mutex::new(HelperProcess {
                child: Some(live_child),
                input: Some(BufWriter::new(closed_input)),
                responses,
                response_reader: Some(response_reader),
            }),
            connected: AtomicBool::new(true),
        };
        assert!(backend.helper.get_mut().unwrap().is_usable());

        backend.disconnect().unwrap();

        assert!(!backend.connected.load(Ordering::Acquire));
    }

    #[test]
    fn disconnect_propagates_a_response_error_from_a_usable_helper() {
        let mut child = Command::new("/bin/cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (response_tx, responses) = mpsc::sync_channel(1);
        let response_reader = thread::spawn(move || helper_response_reader(output, response_tx));
        let mut backend = MacRfcommBackend {
            address: "AA:BB:CC:DD:EE:FF".to_owned(),
            channel: None,
            helper: Mutex::new(HelperProcess {
                child: Some(child),
                input: Some(BufWriter::new(input)),
                responses,
                response_reader: Some(response_reader),
            }),
            connected: AtomicBool::new(true),
        };

        let error = backend.disconnect().unwrap_err();

        assert!(format!("{error:#}").contains("response is too short"));
        assert!(!backend.connected.load(Ordering::Acquire));
        assert!(backend.helper.get_mut().unwrap().is_usable());
    }

    #[test]
    fn disconnect_propagates_helper_eof_after_writing_the_request() {
        let mut child = Command::new("/bin/dd")
            .args(["of=/dev/null", "bs=5", "count=1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (response_tx, responses) = mpsc::sync_channel(1);
        let response_reader = thread::spawn(move || helper_response_reader(output, response_tx));
        let mut backend = MacRfcommBackend {
            address: "AA:BB:CC:DD:EE:FF".to_owned(),
            channel: None,
            helper: Mutex::new(HelperProcess {
                child: Some(child),
                input: Some(BufWriter::new(input)),
                responses,
                response_reader: Some(response_reader),
            }),
            connected: AtomicBool::new(true),
        };

        let error = backend.disconnect().unwrap_err();

        assert!(format!("{error:#}").contains("closed stdout before replying"));
        assert!(!backend.connected.load(Ordering::Acquire));
    }

    #[test]
    fn helper_state_requires_a_running_child_and_live_io_resources() {
        assert!(helper_state_is_usable(
            HelperChildState::Running,
            true,
            HelperResponseReaderState::Running
        ));
        assert!(!helper_state_is_usable(
            HelperChildState::Exited,
            true,
            HelperResponseReaderState::Running
        ));
        assert!(!helper_state_is_usable(
            HelperChildState::Running,
            false,
            HelperResponseReaderState::Running
        ));
        assert!(!helper_state_is_usable(
            HelperChildState::Running,
            true,
            HelperResponseReaderState::Finished
        ));
    }

    #[test]
    fn connect_retry_requires_a_failed_request_and_an_unusable_helper() {
        assert!(should_retry_connect(true, false));
        assert!(!should_retry_connect(true, true));
        assert!(!should_retry_connect(false, false));
        assert!(!should_retry_connect(false, true));
    }

    #[test]
    fn backend_is_send_without_moving_objective_c_objects() {
        fn assert_send<T: Send>() {}
        assert_send::<MacRfcommBackend>();
    }
}
