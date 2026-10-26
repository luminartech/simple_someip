use thiserror::Error;

/// Errors that can occur during SOME/IP client operations.
///
/// # Stability
///
/// This enum is `#[non_exhaustive]`, so downstream crates must include a
/// wildcard arm when matching on it and a new variant is *not* a breaking
/// change. Renaming or restructuring an existing variant still is, and still
/// needs a changelog entry and a `SemVer` bump.
///
/// The attribute was added deliberately: this crate is pre-1.0 with an active
/// release cadence, and without it every added variant broke every downstream
/// `match` — including consumers who only ever wanted a catch-all.
#[derive(Error, Debug)]
#[non_exhaustive]
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
    /// A fixed-capacity internal structure is full.
    ///
    /// [`CapacityKind`](crate::CapacityKind) names which one, and its variant docs name the
    /// governing compile-time constant. Before 0.12.0 this carried a
    /// `&'static str` tag and the docs told you to grep the crate for it;
    /// the `Display` output is unchanged.
    #[error("internal capacity exceeded: {0}")]
    Capacity(crate::CapacityKind),
    /// An error surfaced by the pluggable transport backend (see
    /// [`crate::transport::TransportError`]).
    #[error(transparent)]
    Transport(#[from] crate::transport::TransportError),
    /// A reply that is an ERROR message, or a RESPONSE whose return code is
    /// not [`ReturnCode::Ok`](crate::protocol::ReturnCode::Ok). The request it
    /// answers resolves to this error, without the reply's payload. A reply
    /// that answers no pending request arrives as
    /// [`ClientUpdate::ErrorReply`](super::ClientUpdate::ErrorReply).
    #[error(
        "{message_type:?} reply with return code 0x{:02X} for request ID 0x{request_id:08X}",
        .return_code.as_u8()
    )]
    ErrorReply {
        /// The reply's request ID, which names the request it answers.
        request_id: u32,
        /// The reply's message type:
        /// [`MessageType::Error`](crate::protocol::MessageType::Error) or
        /// [`MessageType::Response`](crate::protocol::MessageType::Response).
        message_type: crate::protocol::MessageType,
        /// The return code the reply carried.
        return_code: crate::protocol::ReturnCode,
        /// What the client's [`E2EHandle`](crate::E2EHandle) check found,
        /// if it checked the reply. After a failed check the message type and
        /// return code may themselves be corrupted.
        e2e_status: Option<crate::E2EStatus>,
    },
    /// A reply failed the client's [`E2EHandle`](crate::E2EHandle) check.
    /// The request it answers resolves to this error, without the reply's
    /// payload.
    #[error("reply failed its E2E check with status {status}")]
    E2eCheckFailed {
        /// The hook's status code.
        status: core::num::NonZeroU8,
    },
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

impl Error {
    /// The [`Error::ErrorReply`] a message with `header` is, if it is one,
    /// with what the E2E check found for it.
    pub(crate) fn error_reply(
        header: &crate::protocol::Header,
        e2e_status: Option<crate::E2EStatus>,
    ) -> Option<Self> {
        use crate::protocol::{MessageType, ReturnCode};
        let message_type = header.message_type().message_type();
        let return_code = header.return_code();
        let is_error = message_type == MessageType::Error
            || (message_type == MessageType::Response && return_code != ReturnCode::Ok);
        is_error.then(|| Error::ErrorReply {
            request_id: header.request_id(),
            message_type,
            return_code,
            e2e_status,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CapacityKind;
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
        let err = Error::Capacity(CapacityKind::RequestQueue);
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
