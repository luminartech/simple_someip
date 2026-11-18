use thiserror::Error;

/// Errors that can occur during SOME/IP server operations.
#[derive(Error, Debug)]
#[non_exhaustive]
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
    /// A fixed-capacity internal structure is full (e.g. a stack send
    /// buffer smaller than the outgoing message).
    ///
    /// [`CapacityKind`](crate::CapacityKind) names which one. The server
    /// currently only produces
    /// [`UdpBuffer`](crate::CapacityKind::UdpBuffer); the kind is shared
    /// with the client so consumers handle capacity exhaustion the same
    /// way whichever layer reports it.
    #[error("internal capacity exceeded: {0}")]
    Capacity(crate::CapacityKind),
    /// A `Server` API was called in a way that violates its
    /// preconditions. The argument is a `&'static str` tag naming the
    /// misuse; current tags:
    /// - `"passive_server_run"` — the run future of a passive server was
    ///   polled. A passive server's SD receive is driven externally.
    /// - `"server_already_running"` — a second run future of one server was
    ///   polled. Two run futures cannot share the sockets and session counter.
    /// - `"new_with_handles_local_port_mismatch"`,
    ///   `"new_passive_with_handles_local_port_mismatch"` — a nonzero
    ///   [`Offering::local_port`](super::Offering::local_port) is not the
    ///   port of the unicast socket handed in.
    /// - `"passive_server_without_offering"` — a passive server was built
    ///   from a config with no [`Offering`](super::Offering).
    #[error("invalid server usage: {0}")]
    InvalidUsage(&'static str),
    /// [`ServerConfig::sd_multicast`](super::ServerConfig::sd_multicast) is
    /// not a multicast group with a nonzero port.
    #[error("SD address {0} is not a multicast group with a nonzero port")]
    InvalidSdMulticast(core::net::SocketAddrV4),
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
