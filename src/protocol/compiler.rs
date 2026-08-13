use super::ProtocolError;
use super::command::Command;
use image::RgbImage;

/// Describes how a session should interpret the printer's reply to a
/// command. Carrying this alongside the encoded bytes means callers no
/// longer need to know, at each call site, which predicate and timeout
/// apply to which command — that knowledge is decided once, in [`compile`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseContract {
    /// Fire-and-forget: nothing is read back.
    None,
    /// Expect an exact `OK` acknowledgement.
    Ok,
    /// Expect exactly one status byte.
    StatusByte,
    /// Expect the asynchronous stop acknowledgement.
    StopAck,
    /// Read whatever bytes come back once, with no framing or validation.
    /// Used by diagnostic queries (density, battery, model, ...) whose
    /// replies vary in length and are interpreted by the caller.
    Raw,
}

/// The wire-ready output of compiling a [`Command`]: encoded bytes plus the
/// contract describing how to read the printer's reply. This is what a
/// [`crate::transport::Transport`] actually sends.
#[derive(Debug, Clone)]
pub struct CompiledCommand {
    pub bytes: Vec<u8>,
    pub response: ResponseContract,
}

/// Compiles a [`Command`] into the exact bytes the D1X protocol expects,
/// paired with the [`ResponseContract`] that governs it. This is the single
/// place that decides both a command's byte layout and which response it
/// expects — previously that knowledge was split between free functions in
/// `protocol` and ad hoc read/predicate logic duplicated across
/// `session.rs` call sites.
pub fn compile(command: Command<'_>) -> Result<CompiledCommand, ProtocolError> {
    let (bytes, response) = match command {
        Command::EnablePrinter => (vec![0x10, 0xff, 0xf1, 0x03], ResponseContract::None),
        Command::WakePrinter => (vec![0u8; 12], ResponseContract::None),
        Command::QueryStatus => (vec![0x10, 0xff, 0x40], ResponseContract::StatusByte),
        Command::SetDensity(density) => (
            vec![0x10, 0xff, 0x10, 0x00, u8::from(density)],
            ResponseContract::Ok,
        ),
        Command::SetAutoShutdown(minutes) => {
            let [high, low] = minutes.to_be_bytes();
            (vec![0x10, 0xff, 0x12, high, low], ResponseContract::Ok)
        }
        Command::GetDensity => (vec![0x10, 0xff, 0x11], ResponseContract::Raw),
        Command::FeedDots(dots) => (vec![0x1b, 0x4a, dots], ResponseContract::None),
        Command::GetAutoShutdown => (vec![0x10, 0xff, 0x13], ResponseContract::Raw),
        Command::StopPrintJob => (vec![0x10, 0xff, 0xf1, 0x45], ResponseContract::StopAck),
        Command::PrintRaster(image) => (encode_raster(image)?, ResponseContract::None),
        Command::GetBatteryLevel => (vec![0x10, 0xff, 0x50, 0xf1], ResponseContract::Raw),
        Command::GetModel => (vec![0x10, 0xff, 0x20, 0xf0], ResponseContract::Raw),
        Command::GetSerialNumber => (vec![0x10, 0xff, 0x20, 0xf2], ResponseContract::Raw),
        Command::GetFirmwareVersion => (vec![0x10, 0xff, 0x20, 0xf1], ResponseContract::Raw),
        Command::FactoryReset => (vec![0x10, 0xff, 0x04], ResponseContract::Ok),
    };

    Ok(CompiledCommand { bytes, response })
}

/// Validates and bit-packs an image into a D1X raster frame (GS v 0 header
/// followed by MSB-first, zero-padded rows).
fn encode_raster(image: &RgbImage) -> Result<Vec<u8>, ProtocolError> {
    let width = image.width();
    let height = image.height();

    if width == 0 || height == 0 {
        return Err(ProtocolError::EmptyImage);
    }

    let row_bytes = (u64::from(width) + 7) >> 3;
    if row_bytes > u64::from(u16::MAX) {
        return Err(ProtocolError::WidthOutOfRange(width));
    }
    if height > u32::from(u16::MAX) {
        return Err(ProtocolError::HeightOutOfRange(height));
    }

    let row_bytes = row_bytes as u16;
    let height = height as u16;
    let payload_len = usize::from(row_bytes)
        .checked_mul(usize::from(height))
        .ok_or(ProtocolError::EncodedImageTooLarge)?;
    let encoded_len = 8usize
        .checked_add(payload_len)
        .ok_or(ProtocolError::EncodedImageTooLarge)?;

    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(encoded_len)
        .map_err(|_| ProtocolError::EncodedImageTooLarge)?;
    encoded.extend_from_slice(&[0x1d, 0x76, 0x30, 0x00]);
    encoded.extend_from_slice(&row_bytes.to_le_bytes());
    encoded.extend_from_slice(&height.to_le_bytes());

    for row in image.rows() {
        let mut packed = 0u8;
        for (x, pixel) in row.enumerate() {
            let channel_sum = u16::from(pixel[0]) + u16::from(pixel[1]) + u16::from(pixel[2]);
            if channel_sum < 3 * 128 {
                packed |= 0x80 >> (x % 8);
            }
            if x % 8 == 7 {
                encoded.push(packed);
                packed = 0;
            }
        }
        if width & 7 != 0 {
            encoded.push(packed);
        }
    }

    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{DEFAULT_AUTO_SHUTDOWN_MINUTES, DEFAULT_FEED_DOTS, Density};
    use image::{Rgb, RgbImage};

    fn bytes(command: Command<'_>) -> Vec<u8> {
        compile(command).unwrap().bytes
    }

    #[test]
    fn command_bytes_match_d1x_protocol() {
        assert_eq!(bytes(Command::EnablePrinter), [0x10, 0xff, 0xf1, 0x03]);
        assert_eq!(bytes(Command::WakePrinter), [0x00; 12]);
        assert_eq!(bytes(Command::QueryStatus), [0x10, 0xff, 0x40]);
        assert_eq!(
            bytes(Command::SetDensity(Density::Light)),
            [0x10, 0xff, 0x10, 0x00, 0]
        );
        assert_eq!(
            bytes(Command::SetDensity(Density::Normal)),
            [0x10, 0xff, 0x10, 0x00, 1]
        );
        assert_eq!(
            bytes(Command::SetDensity(Density::Dark)),
            [0x10, 0xff, 0x10, 0x00, 2]
        );
        assert_eq!(
            bytes(Command::SetAutoShutdown(DEFAULT_AUTO_SHUTDOWN_MINUTES)),
            [0x10, 0xff, 0x12, 0x00, 0x00]
        );
        assert_eq!(
            bytes(Command::FeedDots(DEFAULT_FEED_DOTS)),
            [0x1b, 0x4a, 0x50]
        );
        assert_eq!(bytes(Command::StopPrintJob), [0x10, 0xff, 0xf1, 0x45]);
        assert_eq!(bytes(Command::GetDensity), [0x10, 0xff, 0x11]);
        assert_eq!(bytes(Command::GetAutoShutdown), [0x10, 0xff, 0x13]);
        assert_eq!(bytes(Command::GetBatteryLevel), [0x10, 0xff, 0x50, 0xf1]);
        assert_eq!(bytes(Command::GetModel), [0x10, 0xff, 0x20, 0xf0]);
        assert_eq!(bytes(Command::GetSerialNumber), [0x10, 0xff, 0x20, 0xf2]);
        assert_eq!(bytes(Command::GetFirmwareVersion), [0x10, 0xff, 0x20, 0xf1]);
        assert_eq!(bytes(Command::FactoryReset), [0x10, 0xff, 0x04]);
    }

    #[test]
    fn auto_shutdown_minutes_use_big_endian_u16_encoding() {
        assert_eq!(
            bytes(Command::SetAutoShutdown(1)),
            [0x10, 0xff, 0x12, 0x00, 0x01]
        );
        assert_eq!(
            bytes(Command::SetAutoShutdown(256)),
            [0x10, 0xff, 0x12, 0x01, 0x00]
        );
        assert_eq!(
            bytes(Command::SetAutoShutdown(u16::MAX)),
            [0x10, 0xff, 0x12, 0xff, 0xff]
        );
    }

    #[test]
    fn commands_declare_the_expected_response_contract() {
        assert_eq!(
            compile(Command::EnablePrinter).unwrap().response,
            ResponseContract::None
        );
        assert_eq!(
            compile(Command::QueryStatus).unwrap().response,
            ResponseContract::StatusByte
        );
        assert_eq!(
            compile(Command::SetDensity(Density::Normal))
                .unwrap()
                .response,
            ResponseContract::Ok
        );
        assert_eq!(
            compile(Command::StopPrintJob).unwrap().response,
            ResponseContract::StopAck
        );
        assert_eq!(
            compile(Command::GetDensity).unwrap().response,
            ResponseContract::Raw
        );
        assert_eq!(
            compile(Command::GetBatteryLevel).unwrap().response,
            ResponseContract::Raw
        );
        assert_eq!(
            compile(Command::FactoryReset).unwrap().response,
            ResponseContract::Ok
        );
    }

    #[test]
    fn raster_encodes_alternating_pixels_msb_first() {
        let mut image = RgbImage::new(8, 1);
        for x in 0..8 {
            let value = if x % 2 == 0 { 0 } else { 255 };
            image.put_pixel(x, 0, Rgb([value, value, value]));
        }

        assert_eq!(
            bytes(Command::PrintRaster(&image)),
            vec![0x1d, 0x76, 0x30, 0x00, 0x01, 0x00, 0x01, 0x00, 0xaa]
        );
    }

    #[test]
    fn raster_pads_partial_row_with_white_bits() {
        let image = RgbImage::from_pixel(9, 1, Rgb([0, 0, 0]));

        assert_eq!(
            bytes(Command::PrintRaster(&image)),
            vec![0x1d, 0x76, 0x30, 0x00, 0x02, 0x00, 0x01, 0x00, 0xff, 0x80]
        );
    }

    #[test]
    fn raster_uses_little_endian_dimensions_and_row_major_order() {
        let mut image = RgbImage::from_pixel(2041, 2, Rgb([255, 255, 255]));
        image.put_pixel(0, 0, Rgb([0, 0, 0]));
        image.put_pixel(2040, 1, Rgb([0, 0, 0]));

        let encoded = bytes(Command::PrintRaster(&image));
        assert_eq!(
            &encoded[..8],
            &[0x1d, 0x76, 0x30, 0x00, 0x00, 0x01, 0x02, 0x00]
        );
        assert_eq!(encoded.len(), 8 + 256 * 2);
        assert_eq!(encoded[8], 0x80);
        assert!(encoded[9..8 + 256].iter().all(|byte| *byte == 0));
        assert!(encoded[8 + 256..8 + 511].iter().all(|byte| *byte == 0));
        assert_eq!(encoded[8 + 511], 0x80);
    }

    #[test]
    fn raster_threshold_uses_average_rgb_below_128() {
        let mut image = RgbImage::new(2, 1);
        image.put_pixel(0, 0, Rgb([127, 128, 128]));
        image.put_pixel(1, 0, Rgb([128, 128, 128]));

        let encoded = bytes(Command::PrintRaster(&image));
        assert_eq!(encoded[8], 0x80);
    }

    #[test]
    fn raster_rejects_zero_sized_images() {
        assert_eq!(
            compile(Command::PrintRaster(&RgbImage::new(0, 1))).unwrap_err(),
            ProtocolError::EmptyImage
        );
        assert_eq!(
            compile(Command::PrintRaster(&RgbImage::new(1, 0))).unwrap_err(),
            ProtocolError::EmptyImage
        );
    }

    #[test]
    fn raster_rejects_dimensions_that_do_not_fit_header() {
        let too_wide = RgbImage::new(u32::from(u16::MAX) * 8 + 1, 1);
        assert_eq!(
            compile(Command::PrintRaster(&too_wide)).unwrap_err(),
            ProtocolError::WidthOutOfRange(524_281)
        );

        let too_tall = RgbImage::new(1, u32::from(u16::MAX) + 1);
        assert_eq!(
            compile(Command::PrintRaster(&too_tall)).unwrap_err(),
            ProtocolError::HeightOutOfRange(65_536)
        );
    }
}
