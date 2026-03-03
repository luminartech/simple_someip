use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("I/O error: {0:?}")]
    Io(embedded_io::ErrorKind),
    #[error("Unexpected end of input")]
    UnexpectedEof,
    #[error("Invalid protocol version: {0:X}")]
    InvalidProtocolVersion(u8),
    #[error("Invalid value in MessageType field: {0:X}")]
    InvalidMessageTypeField(u8),
    #[error("Invalid value in ReturnCode field: {0:X}")]
    InvalidReturnCode(u8),
    #[error("Unsupported MessageID  {0:X?}")]
    UnsupportedMessageID(super::MessageId),
    #[error("Invalid value for Service Discovery entry type: {0:X}")]
    InvalidSDEntryType(u8),
    #[error("Invalid value for Service Discovery Option Type: {0:X}")]
    InvalidSDOptionType(u8),
    #[error("Invalid value for Service Discovery Option Transport Protocol: {0:X}")]
    InvalidSDOptionTransportProtocol(u8),
    #[error("Incorrect options size, {0} bytes remaining")]
    IncorrectOptionsSize(usize),
    #[error("Too many SD entries for fixed-capacity buffer")]
    TooManyEntries,
    #[error("Too many SD options for fixed-capacity buffer")]
    TooManyOptions,
    #[error(
        "Invalid SD option length for type 0x{option_type:02X}: expected {expected}, got {actual}"
    )]
    InvalidSDOptionLength {
        option_type: u8,
        expected: u16,
        actual: u16,
    },
    #[error("Configuration string too long: {0} bytes")]
    ConfigurationStringTooLong(usize),
    #[error("Invalid SD message: {0}")]
    InvalidSDMessage(&'static str),
}
