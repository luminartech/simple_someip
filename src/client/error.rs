use thiserror::Error;

/// Errors that can occur during SOME/IP client operations.
///
/// Marked `#[non_exhaustive]` so that future variants (for example, new
/// transport-specific error conditions in upcoming releases) can be added
/// without a further breaking change.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// A SOME/IP protocol-level error.
    #[error(transparent)]
    Protocol(#[from] crate::protocol::Error),
    /// An I/O error from the underlying network transport.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Received a discovery message that was not expected.
    #[error("Unexpected discovery message: {0:?}")]
    UnexpectedDiscoveryMessage(crate::protocol::Header),
    /// A socket was closed unexpectedly.
    #[error("Socket Closed Unexpectedly")]
    SocketClosedUnexpectedly,
    /// The unicast socket has not been bound yet.
    #[error("Unicast Socket not bound")]
    UnicastSocketNotBound,
    /// The requested service was not found in the endpoint registry.
    #[error("Service not found in endpoint registry")]
    ServiceNotFound,
    /// An E2E protection or checking error occurred.
    #[error(transparent)]
    E2e(#[from] crate::e2e::Error),
    /// A fixed-capacity internal structure is full. The argument names the
    /// structure so bare-metal users can size the corresponding compile-time
    /// constant up (e.g. `"unicast_sockets"`).
    #[error("internal capacity exceeded: {0}")]
    Capacity(&'static str),
}
