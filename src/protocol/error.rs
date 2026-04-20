#[cfg(feature = "std")]
use thiserror::Error;

/// Errors that can occur when encoding, decoding, or validating SOME/IP messages.
#[derive(Debug)]
#[cfg_attr(feature = "std", derive(Error))]
pub enum Error {
    /// An I/O error occurred while reading or writing bytes.
    #[cfg_attr(feature = "std", error("I/O error: {0:?}"))]
    Io(embedded_io::ErrorKind),
    /// The input buffer ended before the expected number of bytes could be read.
    #[cfg_attr(feature = "std", error("Unexpected end of input"))]
    UnexpectedEof,
    /// The protocol version field contains an unsupported value.
    #[cfg_attr(feature = "std", error("Invalid protocol version: {0:X}"))]
    InvalidProtocolVersion(u8),
    /// The message type field contains an unrecognized value.
    #[cfg_attr(feature = "std", error("Invalid value in MessageType field: {0:X}"))]
    InvalidMessageTypeField(u8),
    /// The return code field contains an unrecognized value.
    #[cfg_attr(feature = "std", error("Invalid value in ReturnCode field: {0:X}"))]
    InvalidReturnCode(u8),
    /// The message ID is not supported by the payload implementation.
    #[cfg_attr(feature = "std", error("Unsupported MessageID  {0:X?}"))]
    UnsupportedMessageID(super::MessageId),
    /// A service discovery (SD) error occurred.
    #[cfg_attr(feature = "std", error(transparent))]
    Sd(#[cfg_attr(feature = "std", from)] super::sd::Error),
}

#[cfg(not(feature = "std"))]
impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(kind) => write!(f, "I/O error: {kind:?}"),
            Self::UnexpectedEof => write!(f, "Unexpected end of input"),
            Self::InvalidProtocolVersion(v) => write!(f, "Invalid protocol version: {v:X}"),
            Self::InvalidMessageTypeField(v) => {
                write!(f, "Invalid value in MessageType field: {v:X}")
            }
            Self::InvalidReturnCode(v) => write!(f, "Invalid value in ReturnCode field: {v:X}"),
            Self::UnsupportedMessageID(id) => write!(f, "Unsupported MessageID {id:X?}"),
            Self::Sd(e) => write!(f, "{e}"),
        }
    }
}

#[cfg(not(feature = "std"))]
impl From<super::sd::Error> for Error {
    fn from(e: super::sd::Error) -> Self {
        Self::Sd(e)
    }
}
