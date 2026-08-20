//! Names the fixed-capacity internal structures whose exhaustion is
//! reportable through the `Capacity` variant of `client::Error` and
//! `server::Error`.
//!
//! Those are deliberately not intra-doc links: both modules are behind
//! features, and this one is compiled unconditionally, so linking them
//! fails the `--no-default-features` and single-feature doc builds.
//!
//! This lives at the crate root rather than under `client` or `server`
//! because both report through it, and a consumer handling capacity
//! exhaustion should not have to learn two different vocabularies for the
//! same condition. Not every kind is reachable from every error type —
//! the server currently only produces [`CapacityKind::UdpBuffer`] — and
//! that asymmetry is deliberate: one shared kind keeps the handling code
//! uniform as the server grows others.

use core::fmt;

/// Which fixed-capacity internal structure overflowed.
///
/// Each variant names the compile-time constant that governs it, so the
/// bound is reachable from the type rather than by grepping for a string
/// tag.
///
/// `#[non_exhaustive]`: new kinds appear as internal structures gain
/// bounds, and that should not break a downstream `match`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CapacityKind {
    /// Bound by `UNICAST_SOCKETS_CAP`. The client cannot bind a new
    /// ephemeral / requested-port unicast socket because the per-client
    /// cap is exhausted.
    UnicastSockets,

    /// Bound by [`crate::UDP_BUFFER_SIZE`]. An outgoing message was
    /// rejected because the encoded form exceeds the application-level
    /// UDP cap.
    ///
    /// With E2E protect configured for the destination key, the
    /// post-protect payload may add up to the protect profile's overhead
    /// bytes (Profile 1: 4, Profile 4: 16). The pre-encode check uses the
    /// raw size; the post-protect re-check inside the spawned send loop
    /// produces this kind if the protected datagram would overflow the
    /// cap.
    ///
    /// The only kind the server currently produces, where it means a
    /// stack send buffer smaller than the outgoing message.
    UdpBuffer,

    /// Bound by `PENDING_RESPONSES_CAP`. A request was enqueued but the
    /// in-flight response table is full; the request was dropped.
    PendingResponses,

    /// Bound by `REQUEST_QUEUE_CAP`. The client's internal control-message
    /// queue overflowed during a multi-pass `push_front` re-enqueue (e.g.
    /// an auto-bind path).
    ///
    /// Public callers normally hit the bounded(4) control channel first
    /// and either backpressure or fail with `Shutdown`; this kind fires
    /// only in the narrow re-enqueue overflow window.
    RequestQueue,

    /// Bound by `SERVICE_REGISTRY_CAP`. A new service-instance endpoint
    /// cannot be registered because the registry is full.
    ServiceRegistry,
}

impl CapacityKind {
    /// The `snake_case` tag for this kind.
    ///
    /// These are the exact strings the pre-0.11.0 `Capacity(&'static str)`
    /// variant carried, so log output and anything scraping it are
    /// unchanged by the move to a typed kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnicastSockets => "unicast_sockets",
            Self::UdpBuffer => "udp_buffer",
            Self::PendingResponses => "pending_responses",
            Self::RequestQueue => "request_queue",
            Self::ServiceRegistry => "service_registry",
        }
    }
}

impl fmt::Display for CapacityKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of `as_str` is that the `Display` output of a
    /// `Capacity` error is byte-identical to what the `&'static str`
    /// variant produced. Pin the exact strings — a rename here is a
    /// silent behavior change for anyone matching on log text, which is
    /// precisely the fragile pattern this type exists to let them stop
    /// doing.
    #[test]
    fn tags_match_the_pre_0_11_string_literals() {
        assert_eq!(CapacityKind::UnicastSockets.as_str(), "unicast_sockets");
        assert_eq!(CapacityKind::UdpBuffer.as_str(), "udp_buffer");
        assert_eq!(CapacityKind::PendingResponses.as_str(), "pending_responses");
        assert_eq!(CapacityKind::RequestQueue.as_str(), "request_queue");
        assert_eq!(CapacityKind::ServiceRegistry.as_str(), "service_registry");
    }

    /// `Display` must delegate to `as_str` verbatim: both error enums
    /// format the variant as `"internal capacity exceeded: {0}"`, so any
    /// divergence changes the rendered error.
    ///
    /// Formats through [`core::fmt::Write`] into a fixed buffer rather
    /// than `to_string()`, so this test runs in the `--no-default-features`
    /// configuration too — where there is no allocator.
    #[test]
    fn display_delegates_to_as_str() {
        use core::fmt::Write as _;

        /// Writes into a fixed buffer; the longest tag is
        /// `"pending_responses"` at 17 bytes.
        struct Buf {
            bytes: [u8; 32],
            len: usize,
        }

        impl core::fmt::Write for Buf {
            fn write_str(&mut self, s: &str) -> core::fmt::Result {
                let end = self.len + s.len();
                if end > self.bytes.len() {
                    return Err(core::fmt::Error);
                }
                self.bytes[self.len..end].copy_from_slice(s.as_bytes());
                self.len = end;
                Ok(())
            }
        }

        for kind in [
            CapacityKind::UnicastSockets,
            CapacityKind::UdpBuffer,
            CapacityKind::PendingResponses,
            CapacityKind::RequestQueue,
            CapacityKind::ServiceRegistry,
        ] {
            let mut buf = Buf {
                bytes: [0; 32],
                len: 0,
            };
            write!(buf, "{kind}").expect("tag must fit the buffer");
            let rendered = core::str::from_utf8(&buf.bytes[..buf.len]).expect("tags are ASCII");
            assert_eq!(rendered, kind.as_str());
        }
    }
}
