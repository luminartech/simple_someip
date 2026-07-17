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
    /// The SOME/IP length field was smaller than the 8-byte minimum (`request_id..return_code`).
    #[error("Invalid SOME/IP length field: {0} (minimum 8)")]
    InvalidLength(u32),
    /// The message ID is not supported by the payload implementation.
    #[error("Unsupported MessageID  {0:X?}")]
    UnsupportedMessageID(super::MessageId),
    /// A service discovery (SD) error occurred.
    #[error(transparent)]
    Sd(#[from] super::sd::Error),
}

impl From<embedded_io::ErrorKind> for Error {
    fn from(k: embedded_io::ErrorKind) -> Self {
        Error::Io(k)
    }
}

impl From<automotive_wire_codec::EncodeToSliceError<Error>> for Error {
    fn from(e: automotive_wire_codec::EncodeToSliceError<Error>) -> Self {
        use automotive_wire_codec::EncodeToSliceError::{Encode, InsufficientBuffer};
        match e {
            InsufficientBuffer(ib) => Error::InsufficientBuffer(ib),
            Encode(inner) => inner,
        }
    }
}

/// Bridges [`crate::e2e::Error`] onto `protocol::Error` so E2E failures can be
/// reported through the same error type as the rest of the wire path.
///
/// E2E deliberately does **not** implement `Encode`/`Decode` (see the
/// `src/e2e` module docs), so it keeps its own `Error` type. This impl only
/// aligns the *shape* of that error with `protocol::Error` for callers that
/// want a single error type to propagate; it does not change E2E's
/// protect/check behavior or on-wire bytes.
///
/// # Mapping
///
/// `e2e::Error` currently has exactly one variant:
///
/// - [`crate::e2e::Error::BufferTooSmall`] `{ needed, actual }` → maps to
///   [`Error::InsufficientBuffer`], **not** [`Error::Incomplete`]. Although
///   `Incomplete { needed, available }` has the identical field shape, its
///   semantics are decode-direction ("input ended before enough bytes could
///   be *read*"). `BufferTooSmall` instead means an output slice was too
///   small to hold the bytes E2E `protect` needed to *write* — the same
///   direction as `automotive_wire_codec::InsufficientBuffer` ("An output
///   slice was too small for the bytes an encode needed to write"). That is
///   the semantically correct counterpart, so `needed`/`actual` map directly
///   onto `InsufficientBuffer`'s `needed`/`available` fields.
impl From<crate::e2e::Error> for Error {
    fn from(err: crate::e2e::Error) -> Self {
        match err {
            crate::e2e::Error::BufferTooSmall { needed, actual } => {
                Error::InsufficientBuffer(automotive_wire_codec::InsufficientBuffer {
                    needed,
                    available: actual,
                })
            }
        }
    }
}

#[cfg(test)]
mod e2e_bridge_tests {
    use super::Error;

    #[test]
    fn buffer_too_small_maps_to_insufficient_buffer() {
        let e2e_err = crate::e2e::Error::BufferTooSmall {
            needed: 16,
            actual: 10,
        };
        let mapped: Error = e2e_err.into();
        match mapped {
            Error::InsufficientBuffer(ib) => {
                assert_eq!(ib.needed, 16);
                assert_eq!(ib.available, 10);
            }
            other => panic!("expected Error::InsufficientBuffer, got {other:?}"),
        }
    }
}
