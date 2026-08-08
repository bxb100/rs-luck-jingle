use image::RgbImage;
use std::fmt;

pub const PRINT_WIDTH_DOTS: u32 = 384;
pub const DEFAULT_FEED_DOTS: u8 = 80;
pub const DEFAULT_AUTO_SHUTDOWN_MINUTES: u16 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Density {
    Light = 0,
    Normal = 1,
    Dark = 2,
}

impl TryFrom<u8> for Density {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Light),
            1 => Ok(Self::Normal),
            2 => Ok(Self::Dark),
            value => Err(ProtocolError::InvalidDensity(value)),
        }
    }
}

impl From<Density> for u8 {
    fn from(value: Density) -> Self {
        value as u8
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrinterStatus {
    pub raw: u8,
    pub printing: bool,
    pub cover_open: bool,
    pub paper_out: bool,
    pub low_battery: bool,
    pub charging: bool,
    pub overheated: bool,
}

impl PrinterStatus {
    pub const fn from_byte(raw: u8) -> Self {
        Self {
            raw,
            printing: raw & (1 << 0) != 0,
            cover_open: raw & (1 << 1) != 0,
            paper_out: raw & (1 << 2) != 0,
            low_battery: raw & (1 << 3) != 0,
            charging: raw & (1 << 5) != 0,
            overheated: raw & ((1 << 4) | (1 << 6)) != 0,
        }
    }

    pub const fn is_ready(&self) -> bool {
        !self.printing
            && !self.cover_open
            && !self.paper_out
            && !self.low_battery
            && !self.charging
            && !self.overheated
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    InvalidDensity(u8),
    EmptyImage,
    WidthOutOfRange(u32),
    HeightOutOfRange(u32),
    EncodedImageTooLarge,
    MissingStatusByte,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDensity(value) => write!(formatter, "invalid density: {value}"),
            Self::EmptyImage => formatter.write_str("image dimensions must be non-zero"),
            Self::WidthOutOfRange(width) => {
                write!(
                    formatter,
                    "image row exceeds the 16-bit byte width: {width} pixels"
                )
            }
            Self::HeightOutOfRange(height) => {
                write!(
                    formatter,
                    "image height exceeds the 16-bit limit: {height} pixels"
                )
            }
            Self::EncodedImageTooLarge => formatter.write_str("encoded image is too large"),
            Self::MissingStatusByte => formatter.write_str("status response is empty"),
        }
    }
}

impl std::error::Error for ProtocolError {}

pub const fn enable_printer() -> [u8; 4] {
    [0x10, 0xff, 0xf1, 0x03]
}

pub const fn wake_printer() -> [u8; 12] {
    [0x00; 12]
}

pub const fn query_status() -> [u8; 3] {
    [0x10, 0xff, 0x40]
}

pub const fn set_density(density: Density) -> [u8; 5] {
    [0x10, 0xff, 0x10, 0x00, density as u8]
}

pub const fn set_auto_shutdown(minutes: u16) -> [u8; 5] {
    let [minutes_high, minutes_low] = minutes.to_be_bytes();
    [0x10, 0xff, 0x12, minutes_high, minutes_low]
}

pub const fn feed_dots(dots: u8) -> [u8; 3] {
    [0x1b, 0x4a, dots]
}

pub const fn stop_print_job() -> [u8; 4] {
    [0x10, 0xff, 0xf1, 0x45]
}

pub fn encode_raster(image: &RgbImage) -> Result<Vec<u8>, ProtocolError> {
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

        if image.width() & 7 != 0 {
            encoded.push(packed);
        }
    }

    Ok(encoded)
}

pub fn parse_status(response: &[u8]) -> Result<PrinterStatus, ProtocolError> {
    response
        .first()
        .copied()
        .map(PrinterStatus::from_byte)
        .ok_or(ProtocolError::MissingStatusByte)
}

pub fn is_ok_response(response: &[u8]) -> bool {
    response == b"OK"
}

pub fn is_stop_ack(response: &[u8]) -> bool {
    response.first() == Some(&0xaa) || response.starts_with(b"OK")
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    #[test]
    fn command_bytes_match_d1x_protocol() {
        assert_eq!(enable_printer(), [0x10, 0xff, 0xf1, 0x03]);
        assert_eq!(wake_printer(), [0x00; 12]);
        assert_eq!(query_status(), [0x10, 0xff, 0x40]);
        assert_eq!(set_density(Density::Light), [0x10, 0xff, 0x10, 0x00, 0]);
        assert_eq!(set_density(Density::Normal), [0x10, 0xff, 0x10, 0x00, 1]);
        assert_eq!(set_density(Density::Dark), [0x10, 0xff, 0x10, 0x00, 2]);
        assert_eq!(
            set_auto_shutdown(DEFAULT_AUTO_SHUTDOWN_MINUTES),
            [0x10, 0xff, 0x12, 0x00, 0x00]
        );
        assert_eq!(feed_dots(DEFAULT_FEED_DOTS), [0x1b, 0x4a, 0x50]);
        assert_eq!(stop_print_job(), [0x10, 0xff, 0xf1, 0x45]);
    }

    #[test]
    fn auto_shutdown_minutes_use_big_endian_u16_encoding() {
        assert_eq!(set_auto_shutdown(1), [0x10, 0xff, 0x12, 0x00, 0x01]);
        assert_eq!(set_auto_shutdown(256), [0x10, 0xff, 0x12, 0x01, 0x00]);
        assert_eq!(set_auto_shutdown(u16::MAX), [0x10, 0xff, 0x12, 0xff, 0xff]);
    }

    #[test]
    fn density_rejects_values_outside_supported_range() {
        assert_eq!(Density::try_from(0), Ok(Density::Light));
        assert_eq!(Density::try_from(1), Ok(Density::Normal));
        assert_eq!(Density::try_from(2), Ok(Density::Dark));
        assert_eq!(Density::try_from(3), Err(ProtocolError::InvalidDensity(3)));
        assert_eq!(
            Density::try_from(u8::MAX),
            Err(ProtocolError::InvalidDensity(u8::MAX))
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
            encode_raster(&image),
            Ok(vec![0x1d, 0x76, 0x30, 0x00, 0x01, 0x00, 0x01, 0x00, 0xaa])
        );
    }

    #[test]
    fn raster_pads_partial_row_with_white_bits() {
        let image = RgbImage::from_pixel(9, 1, Rgb([0, 0, 0]));

        assert_eq!(
            encode_raster(&image),
            Ok(vec![
                0x1d, 0x76, 0x30, 0x00, 0x02, 0x00, 0x01, 0x00, 0xff, 0x80,
            ])
        );
    }

    #[test]
    fn raster_uses_little_endian_dimensions_and_row_major_order() {
        let mut image = RgbImage::from_pixel(2041, 2, Rgb([255, 255, 255]));
        image.put_pixel(0, 0, Rgb([0, 0, 0]));
        image.put_pixel(2040, 1, Rgb([0, 0, 0]));

        let encoded = encode_raster(&image).unwrap();
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

        let encoded = encode_raster(&image).unwrap();
        assert_eq!(encoded[8], 0x80);
    }

    #[test]
    fn raster_rejects_zero_sized_images() {
        assert_eq!(
            encode_raster(&RgbImage::new(0, 1)),
            Err(ProtocolError::EmptyImage)
        );
        assert_eq!(
            encode_raster(&RgbImage::new(1, 0)),
            Err(ProtocolError::EmptyImage)
        );
    }

    #[test]
    fn raster_rejects_dimensions_that_do_not_fit_header() {
        let too_wide = RgbImage::new(u32::from(u16::MAX) * 8 + 1, 1);
        assert_eq!(
            encode_raster(&too_wide),
            Err(ProtocolError::WidthOutOfRange(524_281))
        );

        let too_tall = RgbImage::new(1, u32::from(u16::MAX) + 1);
        assert_eq!(
            encode_raster(&too_tall),
            Err(ProtocolError::HeightOutOfRange(65_536))
        );
    }

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
    fn printer_is_ready_only_when_no_blocking_status_is_active() {
        assert!(PrinterStatus::from_byte(0).is_ready());

        for blocking_bit in [0, 1, 2, 3, 4, 5, 6] {
            assert!(!PrinterStatus::from_byte(1 << blocking_bit).is_ready());
        }
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
}
