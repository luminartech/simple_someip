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
#[derive(Error, Debug)]
pub enum Error {
    /// A SOME/IP protocol-level error.
    #[error(transparent)]
    Protocol(#[from] crate::protocol::Error),
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
    /// The key's endpoint uses an address family or transport protocol
    /// the client's transports cannot send to (currently IPv4 + UDP
    /// only). The registry stores such keys without error; sending or
    /// subscribing to them fails with this variant.
    #[error("Endpoint not supported by this client's transports (IPv4/UDP only): {0:?}")]
    UnsupportedEndpoint(crate::NetEndpoint),
    /// An E2E protection or checking error occurred.
    #[error(transparent)]
    E2e(#[from] crate::e2e::Error),
    /// A fixed-capacity internal structure is full. The argument is a
    /// lowercase `snake_case` tag naming the resource; grep the crate for
    /// the tag to find the compile-time constant that governs it.
    ///
    /// Current tags:
    /// - `"unicast_sockets"` — bound by `UNICAST_SOCKETS_CAP`. The
    ///   client cannot bind a new ephemeral / requested-port unicast
    ///   socket because the per-client cap is exhausted.
    /// - `"udp_buffer"` — bound by [`crate::UDP_BUFFER_SIZE`]. A
    ///   `Client::send` was rejected because the encoded message
    ///   exceeds the application-level UDP cap. **Note:** with E2E
    ///   protect configured for the destination key, the post-protect
    ///   payload may add up to the protect profile's overhead bytes
    ///   (Profile 1: 4, Profile 4: 16). The pre-encode check uses the
    ///   raw size; the post-protect re-check inside the spawned send
    ///   loop produces this error if the protected datagram would
    ///   overflow the cap.
    /// - `"pending_responses"` — bound by `PENDING_RESPONSES_CAP`. A
    ///   request was enqueued but the in-flight response table is
    ///   full; the request was dropped.
    /// - `"request_queue"` — bound by `REQUEST_QUEUE_CAP`. The
    ///   client's internal control-message queue overflowed during a
    ///   multi-pass `push_front` re-enqueue (e.g. an auto-bind path).
    ///   Public callers normally hit the bounded(4) control channel
    ///   first and either backpressure or fail with `Shutdown`; this
    ///   tag fires only in the narrow re-enqueue overflow window.
    /// - `"service_registry"` — bound by `SERVICE_REGISTRY_CAP`. A
    ///   new `(service_id, instance_id)` endpoint cannot be registered
    ///   because the registry is full.
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

    #[test]
    fn capacity_variant_includes_tag_in_display() {
        let err = Error::Capacity("request_queue");
        let displayed = format!("{err}");
        assert!(
            displayed.contains("request_queue"),
            "Capacity display must include the tag: {displayed:?}"
        );
    }

    #[test]
    fn shutdown_variant_display() {
        let err = Error::Shutdown;
        let displayed = format!("{err}");
        assert!(
            !displayed.is_empty(),
            "Shutdown must have a non-empty display message"
        );
    }

    #[test]
    fn simple_variants_display_without_panicking() {
        for err in [
            Error::SocketClosedUnexpectedly,
            Error::UnicastSocketNotBound,
            Error::ServiceNotFound,
            Error::Shutdown,
        ] {
            let _ = format!("{err}");
        }
    }
}
