use thiserror::Error;

/// Errors that can occur during SOME/IP client operations.
///
/// # Stability
///
/// This enum is **not** marked `#[non_exhaustive]`, so downstream crates
/// may currently match it exhaustively. That convenience comes with a
/// real cost: **any new variant added here is a breaking change** and
/// must be flagged in the changelog and reflected in the next SemVer
/// bump (pre-1.0, a minor bump is sufficient, but it still requires a
/// release-notes entry). The same is true of renaming or restructuring
/// existing variants.
///
/// Marking this `#[non_exhaustive]` — so future additions become
/// non-breaking — is planned as part of an explicit breaking release;
/// until then, treat variant additions as breaking and plan the release
/// accordingly.
#[derive(Error, Debug)]
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
    /// A fixed-capacity internal structure is full. The argument is a
    /// lowercase `snake_case` tag naming the resource; grep the crate for
    /// the tag to find the compile-time constant that governs it. Current
    /// tags: `"unicast_sockets"` (→ `UNICAST_SOCKETS_CAP`), `"udp_buffer"`
    /// (→ `crate::UDP_BUFFER_SIZE`).
    #[error("internal capacity exceeded: {0}")]
    Capacity(&'static str),
    /// An error surfaced by the pluggable transport backend (see
    /// [`crate::transport::TransportError`]).
    #[error("transport error: {0:?}")]
    Transport(#[from] crate::transport::TransportError),
}
