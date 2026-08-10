use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::time::Duration;

use anyhow::{Context, Result, bail};

pub const PRINTER_NAME_PREFIX: &str = "LuckP_D1X_";
#[cfg(target_os = "linux")]
const POST_DISCOVERY_DELAY: Duration = Duration::from_millis(150);
#[cfg(target_os = "macos")]
const MAX_DIAGNOSTIC_DEVICES: usize = 8;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrinterCandidate {
    pub name: String,
    pub address: String,
    pub rssi: Option<i16>,
}

#[cfg(target_os = "linux")]
pub async fn discover_printers(scan_timeout: Duration) -> Result<Vec<PrinterCandidate>> {
    use bluer::{AdapterEvent, DiscoveryFilter, DiscoveryTransport};
    use futures_util::{StreamExt, pin_mut};

    let session = bluer::Session::new()
        .await
        .context("failed to connect to the BlueZ D-Bus service")?;
    let adapter = session
        .default_adapter()
        .await
        .context("failed to find a default Bluetooth adapter")?;
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

    adapter
        .set_discovery_filter(DiscoveryFilter {
            transport: DiscoveryTransport::BrEdr,
            pattern: Some(PRINTER_NAME_PREFIX.to_owned()),
            ..DiscoveryFilter::default()
        })
        .await
        .context("failed to configure BR/EDR printer discovery")?;

    let mut candidates = Vec::new();
    {
        let events = adapter
            .discover_devices_with_changes()
            .await
            .context("failed to start BR/EDR printer discovery")?;
        pin_mut!(events);

        let scan = async {
            while let Some(event) = events.next().await {
                let AdapterEvent::DeviceAdded(address) = event else {
                    continue;
                };

                match inspect_candidate(&adapter, address).await {
                    Ok(Some(candidate)) => candidates.push(candidate),
                    Ok(None) => {}
                    Err(error) => {
                        log::debug!(
                            "failed to inspect discovered Bluetooth device {address}: {error:#}"
                        );
                    }
                }
            }
        };

        if tokio::time::timeout(scan_timeout, scan).await.is_ok() {
            log::debug!("Bluetooth discovery stream ended before the scan timeout");
        }
    }
    tokio::time::sleep(POST_DISCOVERY_DELAY).await;

    Ok(normalize_candidates(candidates))
}

#[cfg(target_os = "linux")]
async fn inspect_candidate(
    adapter: &bluer::Adapter,
    address: bluer::Address,
) -> Result<Option<PrinterCandidate>> {
    let device = adapter
        .device(address)
        .with_context(|| format!("failed to open Bluetooth device {address}"))?;
    let Some(rssi) = device
        .rssi()
        .await
        .with_context(|| format!("failed to read RSSI for {address}"))?
    else {
        return Ok(None);
    };
    let Some(name) = device
        .name()
        .await
        .with_context(|| format!("failed to read the name for {address}"))?
    else {
        return Ok(None);
    };
    if !name.starts_with(PRINTER_NAME_PREFIX) {
        return Ok(None);
    }

    Ok(Some(PrinterCandidate {
        name,
        address: address.to_string(),
        rssi: Some(rssi),
    }))
}

#[cfg(target_os = "macos")]
pub async fn discover_printers(scan_timeout: Duration) -> Result<Vec<PrinterCandidate>> {
    let devices = crate::macos_bluetooth::discover_devices(scan_timeout)
        .context("macOS Bluetooth discovery failed")?;
    macos_printer_candidates(devices)
}

#[cfg(target_os = "macos")]
fn macos_printer_candidates(
    devices: Vec<crate::macos_bluetooth::DiscoveredDevice>,
) -> Result<Vec<PrinterCandidate>> {
    let raw_count = devices.len();
    let mut unresolved_count = 0;
    let mut observed = Vec::new();
    let mut candidates = Vec::new();

    for device in devices {
        match device.name {
            Some(name) => {
                log::debug!(
                    "macOS Bluetooth Classic device: name={name:?}, address={}",
                    device.address
                );
                if name.starts_with(PRINTER_NAME_PREFIX) {
                    candidates.push(PrinterCandidate {
                        name,
                        address: device.address,
                        rssi: None,
                    });
                } else if observed.len() < MAX_DIAGNOSTIC_DEVICES {
                    observed.push(format!("{name:?} ({})", device.address));
                }
            }
            None => {
                unresolved_count += 1;
                log::debug!(
                    "macOS Bluetooth Classic device: name=<unresolved>, address={}",
                    device.address
                );
                if observed.len() < MAX_DIAGNOSTIC_DEVICES {
                    observed.push(format!("<name unresolved> ({})", device.address));
                }
            }
        }
    }

    let candidates = normalize_candidates(candidates);
    if !candidates.is_empty() {
        return Ok(candidates);
    }
    if raw_count == 0 {
        bail!(
            "macOS Bluetooth Classic inquiry found no devices; ensure the printer is powered on, disconnected from other hosts, and advertising in BR/EDR discoverable mode"
        );
    }

    let omitted_count = raw_count.saturating_sub(observed.len());
    let mut observed = if observed.is_empty() {
        "none".to_owned()
    } else {
        observed.join(", ")
    };
    if omitted_count > 0 {
        observed.push_str(&format!(", and {omitted_count} more"));
    }
    if unresolved_count == raw_count {
        bail!(
            "macOS Bluetooth Classic inquiry found {raw_count} device(s), but none returned a usable Bluetooth name; observed: {observed}"
        );
    }
    bail!(
        "macOS Bluetooth Classic inquiry found {raw_count} device(s), but none matched {PRINTER_NAME_PREFIX}; {unresolved_count} device name(s) could not be resolved; observed: {observed}"
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub async fn discover_printers(_scan_timeout: Duration) -> Result<Vec<PrinterCandidate>> {
    bail!(
        "Bluetooth Classic printer discovery is unsupported on {}",
        std::env::consts::OS
    )
}

pub fn select_printer<R, W>(
    candidates: &[PrinterCandidate],
    input: &mut R,
    output: &mut W,
) -> Result<PrinterCandidate>
where
    R: BufRead,
    W: Write,
{
    let candidates = normalize_candidates(candidates.iter().cloned());
    match candidates.as_slice() {
        [] => bail!(
            "no printer matching {PRINTER_NAME_PREFIX} was discovered during the current scan"
        ),
        [candidate] => return Ok(candidate.clone()),
        _ => {}
    }

    write_candidate_list(&candidates, output)?;
    write!(output, "Select printer [1-{}]: ", candidates.len())
        .context("failed to write the printer selection prompt")?;
    output
        .flush()
        .context("failed to flush the printer selection prompt")?;

    let mut selection = String::new();
    if input
        .read_line(&mut selection)
        .context("failed to read the printer selection")?
        == 0
    {
        bail!("printer selection input ended before a choice was provided");
    }

    let selected = selection
        .trim()
        .parse::<usize>()
        .ok()
        .and_then(|number| number.checked_sub(1))
        .and_then(|index| candidates.get(index))
        .cloned();
    selected.with_context(|| {
        format!(
            "invalid printer selection {:?}; expected a number from 1 to {}",
            selection.trim(),
            candidates.len()
        )
    })
}

pub fn write_printer_candidates<W>(candidates: &[PrinterCandidate], output: &mut W) -> Result<()>
where
    W: Write,
{
    let candidates = normalize_candidates(candidates.iter().cloned());
    write_candidate_list(&candidates, output)
}

fn write_candidate_list<W>(candidates: &[PrinterCandidate], output: &mut W) -> Result<()>
where
    W: Write,
{
    writeln!(output, "Multiple printers discovered:")
        .context("failed to write the printer selection list")?;
    for (index, candidate) in candidates.iter().enumerate() {
        match candidate.rssi {
            Some(rssi) => writeln!(
                output,
                "{}. {} ({}, RSSI: {rssi} dBm)",
                index + 1,
                candidate.name,
                candidate.address
            ),
            None => writeln!(
                output,
                "{}. {} ({}, RSSI: unknown)",
                index + 1,
                candidate.name,
                candidate.address
            ),
        }
        .context("failed to write a printer selection entry")?;
    }
    output
        .flush()
        .context("failed to flush the printer selection list")
}

fn normalize_candidates(
    candidates: impl IntoIterator<Item = PrinterCandidate>,
) -> Vec<PrinterCandidate> {
    let mut by_address = BTreeMap::new();
    for mut candidate in candidates {
        candidate.address = candidate
            .address
            .trim()
            .replace('-', ":")
            .to_ascii_uppercase();
        match by_address.entry(candidate.address.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(candidate);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let current = entry.get_mut();
                if candidate.name < current.name {
                    current.name = candidate.name;
                }
                current.rssi = current.rssi.max(candidate.rssi);
            }
        }
    }

    let mut candidates: Vec<_> = by_address.into_values().collect();
    candidates.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.address.cmp(&right.address))
    });
    candidates
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn candidate(name: &str, address: &str, rssi: Option<i16>) -> PrinterCandidate {
        PrinterCandidate {
            name: name.to_owned(),
            address: address.to_owned(),
            rssi,
        }
    }

    #[cfg(target_os = "macos")]
    fn macos_device(name: Option<&str>, address: &str) -> crate::macos_bluetooth::DiscoveredDevice {
        crate::macos_bluetooth::DiscoveredDevice {
            name: name.map(str::to_owned),
            address: address.to_owned(),
        }
    }

    #[test]
    fn normalize_deduplicates_by_address_and_sorts_by_name_then_address() {
        let candidates = normalize_candidates([
            candidate("LuckP_D1X_B", "02:00:00:00:00:02", Some(-70)),
            candidate("LuckP_D1X_A", "02:00:00:00:00:03", Some(-60)),
            candidate("LuckP_D1X_B", "02:00:00:00:00:01", None),
            candidate("LuckP_D1X_B", "02:00:00:00:00:02", Some(-40)),
            candidate("LuckP_D1X_B", "02:00:00:00:00:01", Some(-50)),
        ]);

        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].name, "LuckP_D1X_A");
        assert_eq!(candidates[1].address, "02:00:00:00:00:01");
        assert_eq!(candidates[1].rssi, Some(-50));
        assert_eq!(candidates[2].address, "02:00:00:00:00:02");
        assert_eq!(candidates[2].rssi, Some(-40));
    }

    #[test]
    fn normalize_treats_address_case_as_equal() {
        let candidates = normalize_candidates([
            candidate("LuckP_D1X_B", "aa:bb:cc:dd:ee:ff", Some(-60)),
            candidate("LuckP_D1X_A", "AA-BB-CC-DD-EE-FF", Some(-50)),
        ]);

        assert_eq!(
            candidates,
            [candidate("LuckP_D1X_A", "AA:BB:CC:DD:EE:FF", Some(-50))]
        );
    }

    #[test]
    fn select_rejects_an_empty_candidate_list() {
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();

        let error = select_printer(&[], &mut input, &mut output).unwrap_err();

        assert!(error.to_string().contains("no printer matching"));
        assert!(output.is_empty());
    }

    #[test]
    fn select_returns_a_single_candidate_without_prompting() {
        let expected = candidate("LuckP_D1X_A", "02:00:00:00:00:01", Some(-45));
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();

        let selected =
            select_printer(std::slice::from_ref(&expected), &mut input, &mut output).unwrap();

        assert_eq!(selected, expected);
        assert!(output.is_empty());
    }

    #[test]
    fn select_lists_name_and_address_and_uses_one_based_choice() {
        let candidates = [
            candidate("LuckP_D1X_B", "02:00:00:00:00:02", Some(-40)),
            candidate("LuckP_D1X_A", "02:00:00:00:00:01", None),
        ];
        let mut input = Cursor::new(b"2\n");
        let mut output = Vec::new();

        let selected = select_printer(&candidates, &mut input, &mut output).unwrap();
        let output = String::from_utf8(output).unwrap();

        assert_eq!(selected.name, "LuckP_D1X_B");
        assert!(output.contains("1. LuckP_D1X_A (02:00:00:00:00:01, RSSI: unknown)"));
        assert!(output.contains("2. LuckP_D1X_B (02:00:00:00:00:02, RSSI: -40 dBm)"));
        assert!(output.ends_with("Select printer [1-2]: "));
    }

    #[test]
    fn select_rejects_an_out_of_range_choice() {
        let candidates = [
            candidate("LuckP_D1X_A", "02:00:00:00:00:01", None),
            candidate("LuckP_D1X_B", "02:00:00:00:00:02", None),
        ];
        let mut input = Cursor::new(b"3\n");
        let mut output = Vec::new();

        let error = select_printer(&candidates, &mut input, &mut output).unwrap_err();

        assert!(error.to_string().contains("expected a number from 1 to 2"));
    }

    #[test]
    fn select_rejects_end_of_input() {
        let candidates = [
            candidate("LuckP_D1X_A", "02:00:00:00:00:01", None),
            candidate("LuckP_D1X_B", "02:00:00:00:00:02", None),
        ];
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();

        let error = select_printer(&candidates, &mut input, &mut output).unwrap_err();

        assert!(error.to_string().contains("input ended"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_candidates_keep_only_named_prefix_matches() {
        let candidates = macos_printer_candidates(vec![
            macos_device(Some("Speaker"), "AA:BB:CC:DD:EE:01"),
            macos_device(None, "AA:BB:CC:DD:EE:02"),
            macos_device(Some("LuckP_D1X_A"), "AA:BB:CC:DD:EE:03"),
        ])
        .unwrap();

        assert_eq!(
            candidates,
            [candidate("LuckP_D1X_A", "AA:BB:CC:DD:EE:03", None)]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_empty_inquiry_has_an_actionable_error() {
        let error = macos_printer_candidates(Vec::new()).unwrap_err();

        assert!(error.to_string().contains("found no devices"));
        assert!(error.to_string().contains("BR/EDR discoverable mode"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_unnamed_devices_have_a_specific_error() {
        let error =
            macos_printer_candidates(vec![macos_device(None, "AA:BB:CC:DD:EE:02")]).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("none returned a usable Bluetooth name"));
        assert!(message.contains("AA:BB:CC:DD:EE:02"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_name_failures_and_mismatches_are_distinguished() {
        let error = macos_printer_candidates(vec![
            macos_device(Some("Speaker"), "AA:BB:CC:DD:EE:01"),
            macos_device(None, "AA:BB:CC:DD:EE:02"),
        ])
        .unwrap_err();
        let message = error.to_string();

        assert!(message.contains("found 2 device(s)"));
        assert!(message.contains("1 device name(s) could not be resolved"));
        assert!(message.contains("Speaker"));
        assert!(message.contains("AA:BB:CC:DD:EE:02"));
    }
}
