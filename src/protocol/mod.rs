//! D1X printer protocol.
//!
//! Callers build a [`Command`] describing *what* they want the printer to
//! do. [`compile`] turns it into the exact bytes the D1X protocol expects,
//! paired with a [`ResponseContract`] describing how to interpret the
//! printer's reply. All protocol knowledge (opcodes, byte layouts, and
//! which commands expect which responses) lives in one place — `compile` —
//! instead of being re-implemented at each call site in `session`.

mod command;
mod compiler;
mod response;

pub use command::Command;
pub use compiler::{CompiledCommand, ResponseContract, compile};
pub use response::{
    decode_device_text, is_ok_response, is_stop_ack, parse_auto_shutdown_minutes,
    parse_battery_percent, parse_density, parse_status,
};

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
    EmptyResponse,
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
            Self::EmptyResponse => formatter.write_str("printer response is empty"),
        }
    }
}

impl std::error::Error for ProtocolError {}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn printer_is_ready_only_when_no_blocking_status_is_active() {
        assert!(PrinterStatus::from_byte(0).is_ready());

        for blocking_bit in [0, 1, 2, 3, 4, 5, 6] {
            assert!(!PrinterStatus::from_byte(1 << blocking_bit).is_ready());
        }
    }
}
