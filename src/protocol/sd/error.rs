use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Invalid value for Service Discovery entry type: {0:X}")]
    InvalidEntryType(u8),
    #[error("Invalid value for Service Discovery Option Type: {0:X}")]
    InvalidOptionType(u8),
    #[error("Invalid value for Service Discovery Option Transport Protocol: {0:X}")]
    InvalidOptionTransportProtocol(u8),
    #[error("Incorrect options size, {0} bytes remaining")]
    IncorrectOptionsSize(usize),
    #[error("Too many SD entries for fixed-capacity buffer")]
    TooManyEntries,
    #[error("Too many SD options for fixed-capacity buffer")]
    TooManyOptions,
    #[error(
        "Invalid SD option length for type 0x{option_type:02X}: expected {expected}, got {actual}"
    )]
    InvalidOptionLength {
        option_type: u8,
        expected: u16,
        actual: u16,
    },
    #[error("Configuration string too long: {0} bytes")]
    ConfigurationStringTooLong(usize),
    #[error("Invalid SD message: {0}")]
    InvalidMessage(&'static str),
    #[error("Entries array length {0} is not a multiple of entry size (16)")]
    IncorrectEntriesSize(usize),
}
