use super::{Density, PrinterStatus, ProtocolError};

/// Decodes a raw status reply into a [`PrinterStatus`]. Pairs with
/// [`super::ResponseContract::StatusByte`].
pub fn parse_status(response: &[u8]) -> Result<PrinterStatus, ProtocolError> {
    response
        .first()
        .copied()
        .map(PrinterStatus::from_byte)
        .ok_or(ProtocolError::MissingStatusByte)
}

/// Recognizes the exact `OK` acknowledgement expected for
/// [`super::ResponseContract::Ok`] commands.
pub fn is_ok_response(response: &[u8]) -> bool {
    response == b"OK"
}

/// Recognizes the printer's asynchronous stop acknowledgement expected for
/// [`super::ResponseContract::StopAck`].
pub fn is_stop_ack(response: &[u8]) -> bool {
    response.first() == Some(&0xaa) || response.starts_with(b"OK")
}

/// Decodes the reply to [`super::Command::GetDensity`] into a [`Density`].
pub fn parse_density(response: &[u8]) -> Result<Density, ProtocolError> {
    response
        .first()
        .copied()
        .ok_or(ProtocolError::EmptyResponse)
        .and_then(Density::try_from)
}

/// Decodes the reply to [`super::Command::GetAutoShutdown`] into minutes.
///
/// The vendor firmware only ever surfaces the low byte here (mirroring the
/// official Android SDK, which discards the high byte even though
/// `SetAutoShutdown` accepts a full `u16`), so auto-shutdown durations of
/// 256 minutes or more cannot be read back exactly.
pub fn parse_auto_shutdown_minutes(response: &[u8]) -> Result<u8, ProtocolError> {
    response.last().copied().ok_or(ProtocolError::EmptyResponse)
}

/// Decodes the reply to [`super::Command::GetBatteryLevel`]. The first byte
/// is a fixed header; the second is the battery percentage.
pub fn parse_battery_percent(response: &[u8]) -> Result<u8, ProtocolError> {
    response.get(1).copied().ok_or(ProtocolError::EmptyResponse)
}

/// Decodes a free-form text reply (model, serial number, firmware version)
/// into a trimmed string. The firmware has no framing for these replies, so
/// any bytes that arrive are treated as the whole answer; trailing NUL
/// padding and surrounding whitespace are stripped.
pub fn decode_device_text(response: &[u8]) -> String {
    String::from_utf8_lossy(response)
        .trim_matches(|character: char| character == '\0' || character.is_whitespace())
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_parser_maps_all_documented_bits() {
        let status = parse_status(&[0b0111_1111]).unwrap();
        assert_eq!(status.raw, 0b0111_1111);
        assert!(status.printing);
        assert!(status.cover_open);
        assert!(status.paper_out);
        assert!(status.low_battery);
        assert!(status.charging);
        assert!(status.overheated);

        assert!(parse_status(&[1 << 4]).unwrap().overheated);
        assert!(parse_status(&[1 << 6]).unwrap().overheated);
        assert!(!parse_status(&[0]).unwrap().overheated);
        assert!(!parse_status(&[1 << 5]).unwrap().is_ready());
        assert_eq!(parse_status(&[]), Err(ProtocolError::MissingStatusByte));
    }

    #[test]
    fn response_recognition_obeys_exact_and_prefix_rules() {
        assert!(is_ok_response(b"OK"));
        assert!(!is_ok_response(b"OK\r\n"));
        assert!(!is_ok_response(b"OKAY"));
        assert!(!is_ok_response(b""));

        assert!(is_stop_ack(&[0xaa]));
        assert!(is_stop_ack(&[0xaa, 0x00]));
        assert!(is_stop_ack(b"OK"));
        assert!(is_stop_ack(b"OK\r\n"));
        assert!(is_stop_ack(b"OKAY"));
        assert!(!is_stop_ack(b"NO"));
        assert!(!is_stop_ack(b""));
    }

    #[test]
    fn density_reply_maps_the_first_byte() {
        assert_eq!(parse_density(&[0]), Ok(Density::Light));
        assert_eq!(parse_density(&[1]), Ok(Density::Normal));
        assert_eq!(parse_density(&[2]), Ok(Density::Dark));
        assert_eq!(parse_density(&[3]), Err(ProtocolError::InvalidDensity(3)));
        assert_eq!(parse_density(&[]), Err(ProtocolError::EmptyResponse));
    }

    #[test]
    fn auto_shutdown_reply_uses_the_last_byte() {
        assert_eq!(parse_auto_shutdown_minutes(&[5]), Ok(5));
        assert_eq!(parse_auto_shutdown_minutes(&[0x01, 0x2c]), Ok(0x2c));
        assert_eq!(
            parse_auto_shutdown_minutes(&[]),
            Err(ProtocolError::EmptyResponse)
        );
    }

    #[test]
    fn battery_reply_uses_the_second_byte() {
        assert_eq!(parse_battery_percent(&[0xf1, 87]), Ok(87));
        assert_eq!(
            parse_battery_percent(&[0xf1]),
            Err(ProtocolError::EmptyResponse)
        );
        assert_eq!(
            parse_battery_percent(&[]),
            Err(ProtocolError::EmptyResponse)
        );
    }

    #[test]
    fn device_text_strips_nul_padding_and_whitespace() {
        assert_eq!(decode_device_text(b"D1X-KD\0\0"), "D1X-KD");
        assert_eq!(decode_device_text(b"  v1.9.0 \r\n"), "v1.9.0");
        assert_eq!(decode_device_text(&[0xff, 0xfe]), "\u{fffd}\u{fffd}");
    }
}
