use thiserror::Error;

/// Errors that can occur when encoding, decoding, or validating SOME/IP messages.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// An I/O error occurred while reading or writing bytes.
    #[error("I/O error: {0:?}")]
    Io(embedded_io::ErrorKind),
    /// Input ended before the expected number of bytes could be read.
    #[error("incomplete: need {} bytes, have {}", .0.needed, .0.available)]
    Incomplete(#[from] automotive_wire_codec::Incomplete),
    /// Bytes remained after a value that should have consumed the whole buffer.
    #[error("trailing bytes: {} left over", .0.0)]
    Trailing(#[from] automotive_wire_codec::TrailingBytes),
    /// An output slice was too small for the bytes an encode needed to write.
    #[error("insufficient buffer: need {} bytes, have {}", .0.needed, .0.available)]
    InsufficientBuffer(#[from] automotive_wire_codec::InsufficientBuffer),
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
