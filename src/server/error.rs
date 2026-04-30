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
    ///
    /// Gated on `feature = "std"` because [`std::io::Error`] is itself
    /// std-only. Bare-metal consumers receive transport-layer
    /// failures through [`Self::Transport`] instead, which carries a
    /// portable [`crate::transport::IoErrorKind`].
    #[cfg(feature = "std")]
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
    /// A `Server` API was called in a way that violates its
    /// preconditions. The argument is a `&'static str` tag naming the
    /// misuse; current tags:
    /// - `"passive_server_announcement_loop"` — `announcement_loop`
    ///   was called on a server constructed via `new_passive`. Passive
    ///   servers have no real SD socket bound to port 30490, so any
    ///   announcements would go out with an incorrect source port.
    ///   Drive announcements from the client side instead.
    /// - `"announcement_loop_already_started"` — `announcement_loop`
    ///   was called twice on the same server. Two announcement
    ///   futures cannot share the same SD socket and session counter.
    #[error("invalid server usage: {0}")]
    InvalidUsage(&'static str),
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
