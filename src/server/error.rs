use thiserror::Error;

/// Errors that can occur during SOME/IP server operations.
///
/// Not marked `#[non_exhaustive]`: downstream crates that match on this
/// enum rely on exhaustiveness. Variant additions are breaking changes
/// and require a `SemVer` bump.
#[derive(Error, Debug)]
pub enum Error {
    /// A SOME/IP protocol-level error.
    #[error(transparent)]
    Protocol(#[from] crate::protocol::Error),
    /// An I/O error from the underlying network transport.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A transport-layer error from a [`crate::transport::TransportFactory`]
    /// or [`crate::transport::TransportSocket`] operation.
    #[error("transport error: {0}")]
    Transport(#[from] crate::transport::TransportError),
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
