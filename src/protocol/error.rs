use thiserror::Error;

/// Errors that can occur when encoding, decoding, or validating SOME/IP messages.
#[derive(Error, Debug)]
pub enum Error {
    /// An I/O error occurred while reading or writing bytes.
    #[error("I/O error: {0:?}")]
    Io(embedded_io::ErrorKind),
    /// The input buffer ended before the expected number of bytes could be read.
    #[error("Unexpected end of input")]
    UnexpectedEof,
    /// The protocol version field contains an unsupported value.
    #[error("Invalid protocol version: {0:X}")]
    InvalidProtocolVersion(u8),
    /// The message type field contains an unrecognized value.
    #[error("Invalid value in MessageType field: {0:X}")]
    InvalidMessageTypeField(u8),
    /// The return code field contains an unrecognized value.
    #[error("Invalid value in ReturnCode field: {0:X}")]
    InvalidReturnCode(u8),
    /// The message ID is not supported by the payload implementation.
    #[error("Unsupported MessageID  {0:X?}")]
    UnsupportedMessageID(super::MessageId),
    /// A service discovery (SD) error occurred.
    #[error(transparent)]
    Sd(#[from] super::sd::Error),
}
