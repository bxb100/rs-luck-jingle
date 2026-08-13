use super::Density;
use image::RgbImage;

/// A printer command as a caller expresses it: domain-level intent, free of
/// any wire-format detail. This is the "source language" of the protocol
/// compiler pipeline described in the [module docs](super).
#[derive(Debug, Clone, Copy)]
pub enum Command<'a> {
    EnablePrinter,
    WakePrinter,
    QueryStatus,
    SetDensity(Density),
    GetDensity,
    SetAutoShutdown(u16),
    GetAutoShutdown,
    FeedDots(u8),
    StopPrintJob,
    PrintRaster(&'a RgbImage),
    GetBatteryLevel,
    GetModel,
    GetSerialNumber,
    GetFirmwareVersion,
    FactoryReset,
}
