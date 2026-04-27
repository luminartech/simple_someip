use thiserror::Error;

/// Errors that can occur during SOME/IP client operations.
///
/// # Stability
///
/// This enum is **not** marked `#[non_exhaustive]`, so downstream crates
/// may currently match it exhaustively. That convenience comes with a
/// real cost: **any new variant added here is a breaking change** and
/// must be flagged in the changelog and reflected in the next `SemVer`
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
    /// tags:
    /// - `"unicast_sockets"` → `UNICAST_SOCKETS_CAP`
    /// - `"udp_buffer"` → `crate::UDP_BUFFER_SIZE`
    /// - `"pending_responses"` → `PENDING_RESPONSES_CAP`
    /// - `"request_queue"` → `REQUEST_QUEUE_CAP` (returned when the
    ///   client's internal control-message queue is saturated, surfacing
    ///   on every public `Client` method that enqueues a control)
    #[error("internal capacity exceeded: {0}")]
    Capacity(&'static str),
    /// An error surfaced by the pluggable transport backend (see
    /// [`crate::transport::TransportError`]).
    #[error(transparent)]
    Transport(#[from] crate::transport::TransportError),
    /// The client's internal run-loop future has exited — either because
    /// the caller dropped it before or during polling, the executor
    /// cancelled its task, or it returned. All public `Client` methods
    /// that enqueue a control message or await its response return
    /// this variant when the control channel is closed, rather than
    /// panicking on `.unwrap()` of the send / recv result. Treat it as
    /// a caller-side lifecycle error: the `Client` handle has outlived
    /// its driver and further calls on it cannot make progress.
    #[error("client run loop is no longer running")]
    Shutdown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::TransportError;
    use std::format;

    #[test]
    fn transport_variant_displays_via_inner_display_not_debug() {
        // Regression guard: previously `{0:?}` leaked debug formatting
        // (e.g. `AddressInUse`) into user-facing error messages. The
        // `#[error(transparent)]` form delegates fully to the inner
        // `TransportError`'s Display impl.
        let err = Error::Transport(TransportError::AddressInUse);
        let displayed = format!("{err}");

        // No debug-format artifacts: no braces (`AddressInUse` is a unit
        // variant, but struct-like variants would debug-format with
        // braces), no quote-wrapping, no raw variant name from debug.
        assert!(
            !displayed.contains('{'),
            "unexpected `{{` in Display output: {displayed:?}"
        );
        assert!(
            !displayed.contains('}'),
            "unexpected `}}` in Display output: {displayed:?}"
        );
        assert!(
            !displayed.contains('"'),
            "unexpected `\"` in Display output: {displayed:?}"
        );

        // `transparent` delegates to the inner Display verbatim.
        let inner = format!("{}", TransportError::AddressInUse);
        assert_eq!(displayed, inner);
        assert_eq!(displayed, "address in use");
    }
}
