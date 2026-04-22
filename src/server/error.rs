use thiserror::Error;

/// Errors that can occur during SOME/IP server operations.
///
/// Marked `#[non_exhaustive]` so future variants (transport-specific errors
/// in upcoming releases) can be added without a breaking change.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// A SOME/IP protocol-level error.
    #[error(transparent)]
    Protocol(#[from] crate::protocol::Error),
    /// An I/O error from the underlying network transport.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// An E2E protection or checking error occurred.
    #[error(transparent)]
    E2e(#[from] crate::e2e::Error),
    /// A fixed-capacity internal structure is full (e.g. a stack send
    /// buffer smaller than the outgoing message). The argument is a
    /// lowercase `snake_case` tag naming the resource; grep the crate for
    /// the tag to find the compile-time constant that governs it. Current
    /// tags: `"udp_buffer"` (→ `crate::UDP_BUFFER_SIZE`).
    #[error("internal capacity exceeded: {0}")]
    Capacity(&'static str),
}

impl From<crate::protocol::sd::Error> for Error {
    fn from(err: crate::protocol::sd::Error) -> Self {
        Self::Protocol(crate::protocol::Error::from(err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_sd_error_produces_protocol_variant() {
        let sd_err = crate::protocol::sd::Error::IncorrectEntriesSize(0);
        let err: Error = sd_err.into();
        assert!(matches!(err, Error::Protocol(_)));
    }
}
