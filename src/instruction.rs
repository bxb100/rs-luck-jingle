use btleplug::api::bleuuid::uuid_from_u16;
use lazy_static::lazy_static;
use uuid::Uuid;
use crate::hex::decode_hex;


//  printer name
pub const PRINTER_NAME_PREFIX: &str = "LuckP_D1";
pub const PRINTER_WIDTH: u32 = 384;

// characters
pub const READ_UUID_1: Uuid = uuid_from_u16(0xFF01);
pub const READ_UUID_2: Uuid = uuid_from_u16(0xFF03);
pub const WRITE_UUID: Uuid = uuid_from_u16(0xFF02);

// command

lazy_static! {
    pub static ref CHECK_MAC_ADDRESS: Vec<u8> = "10 FF 30 12".to_hex();
    pub static ref DISABLE_SHUTDOWN: Vec<u8> = "10 FF 12 00 00".to_hex();
    pub static ref ENABLE_PRINTER: Vec<u8> = "10 FF F1 03".to_hex();
    pub static ref SET_THICKNESS: Vec<u8> = "10 FF 10 00 03".to_hex();
    pub static ref PRINT_LINE_DOTS: Vec<u8> = "1B 4A 40".to_hex();
    pub static ref STOP_PRINT_JOBS: Vec<u8> = "10 FF F1 45".to_hex();
    pub static ref IMAGE_COMMAND_HEADER: Vec<u8> = "1D 76 30".to_hex();

    pub static ref PRINTER_WAKE_MAGIC_END: Vec<u8> = "00".repeat(3096).to_hex();
}

trait EnhanceString {
    fn to_hex(&self) -> Vec<u8>;
}

impl EnhanceString for str {
    fn to_hex(&self) -> Vec<u8> {
        decode_hex(&self.replace(' ', "")).unwrap()
    }
}

pub struct BLEMessage {
    pub payload: Vec<u8>,
}

