//! Shared network-endpoint identity types.
//!
//! [`NetEndpoint`] is the crate's transport-layer identity: the full spec
//! socket plus the transport protocol. AUTOSAR `PRS_SOMEIP` §4.2.1.3
//! identifies a service instance "through the combination of the Service
//! ID combined with the socket (i.e. IP-address, transport protocol, and
//! port number)" — this type is the socket half of that pair. It carries
//! no application-layer discriminator (service id, instance id,
//! eventgroup), so sibling protocol crates can share the same shape.

use core::net::SocketAddr;

/// Transport protocol of a network endpoint.
///
/// `Udp`/`Tcp` correspond to the IANA protocol numbers SOME/IP-SD
/// endpoint options carry on the wire (0x11 / 0x06). `Tls` (secure `DoIP`,
/// ISO 13400-2 Ed. 3) has no IANA protocol number and therefore no
/// SOME/IP-SD wire encoding; encoding it into an SD option fails with
/// [`crate::protocol::sd::Error::UnencodableTransportProtocol`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum TransportProtocol {
    /// UDP (IANA 0x11).
    Udp,
    /// TCP (IANA 0x06).
    Tcp,
    /// TLS over TCP. No IANA protocol number; not representable in
    /// SOME/IP-SD endpoint options.
    Tls,
}

/// A full transport endpoint: socket address plus transport protocol.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NetEndpoint {
    /// IP address and port.
    pub addr: SocketAddr,
    /// Transport protocol on that socket.
    pub protocol: TransportProtocol,
}

impl NetEndpoint {
    #[must_use]
    pub const fn new(addr: SocketAddr, protocol: TransportProtocol) -> Self {
        Self { addr, protocol }
    }

    #[must_use]
    pub const fn udp(addr: SocketAddr) -> Self {
        Self::new(addr, TransportProtocol::Udp)
    }

    #[must_use]
    pub const fn tcp(addr: SocketAddr) -> Self {
        Self::new(addr, TransportProtocol::Tcp)
    }

    #[must_use]
    pub const fn tls(addr: SocketAddr) -> Self {
        Self::new(addr, TransportProtocol::Tls)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::net::{Ipv4Addr, SocketAddrV4};

    #[test]
    fn constructors_set_protocol() {
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 30490));
        assert_eq!(NetEndpoint::udp(addr).protocol, TransportProtocol::Udp);
        assert_eq!(NetEndpoint::tcp(addr).protocol, TransportProtocol::Tcp);
        assert_eq!(NetEndpoint::tls(addr).protocol, TransportProtocol::Tls);
        assert_eq!(NetEndpoint::udp(addr).addr, addr);
    }

    #[test]
    fn endpoints_differing_only_in_protocol_are_distinct() {
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 30490));
        assert_ne!(NetEndpoint::udp(addr), NetEndpoint::tcp(addr));
    }
}
