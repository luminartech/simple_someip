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
    #[error(transparent)]
    Sd(#[from] super::sd::Error),
}
