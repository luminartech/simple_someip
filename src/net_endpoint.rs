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
#[cfg(any(feature = "client", feature = "server"))]
use core::net::{Ipv4Addr, SocketAddrV4};

/// Transport protocol of a network endpoint.
///
/// `Udp`/`Tcp` correspond to the IANA protocol numbers SOME/IP-SD
/// endpoint options carry on the wire (0x11 / 0x06) — the only two
/// transport protocols the SOME/IP specification defines for endpoint
/// options.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum TransportProtocol {
    /// UDP (IANA 0x11).
    Udp,
    /// TCP (IANA 0x06).
    Tcp,
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
}

/// The endpoint a peer is reached at, chosen from the IPv4 endpoint options of
/// one SD entry: the first UDP endpoint, else the first TCP one, else `None`.
#[cfg(any(feature = "client", feature = "server"))]
pub(crate) fn preferred_ipv4_endpoint(
    endpoints: impl Iterator<Item = (Ipv4Addr, TransportProtocol, u16)>,
) -> Option<NetEndpoint> {
    let mut tcp = None;
    for (ip, protocol, port) in endpoints {
        let endpoint = NetEndpoint::new(SocketAddr::V4(SocketAddrV4::new(ip, port)), protocol);
        if protocol == TransportProtocol::Udp {
            return Some(endpoint);
        }
        tcp.get_or_insert(endpoint);
    }
    tcp
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::net::{Ipv4Addr, SocketAddrV4};

    #[cfg(any(feature = "client", feature = "server"))]
    #[test]
    fn prefers_udp_over_an_earlier_tcp_endpoint() {
        let tcp = (Ipv4Addr::new(10, 0, 0, 1), TransportProtocol::Tcp, 30501);
        let udp = (Ipv4Addr::new(10, 0, 0, 2), TransportProtocol::Udp, 30502);
        let chosen = preferred_ipv4_endpoint([tcp, udp].into_iter());
        assert_eq!(
            chosen,
            Some(NetEndpoint::udp(SocketAddr::V4(SocketAddrV4::new(
                udp.0, udp.2
            ))))
        );
    }

    #[cfg(any(feature = "client", feature = "server"))]
    #[test]
    fn falls_back_to_the_first_tcp_endpoint() {
        let first = (Ipv4Addr::new(10, 0, 0, 1), TransportProtocol::Tcp, 30501);
        let second = (Ipv4Addr::new(10, 0, 0, 2), TransportProtocol::Tcp, 30502);
        let chosen = preferred_ipv4_endpoint([first, second].into_iter());
        assert_eq!(
            chosen,
            Some(NetEndpoint::tcp(SocketAddr::V4(SocketAddrV4::new(
                first.0, first.2
            ))))
        );
    }

    #[cfg(any(feature = "client", feature = "server"))]
    #[test]
    fn no_ipv4_endpoint_is_none() {
        assert_eq!(preferred_ipv4_endpoint(core::iter::empty()), None);
    }

    #[test]
    fn constructors_set_protocol() {
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 30490));
        assert_eq!(NetEndpoint::udp(addr).protocol, TransportProtocol::Udp);
        assert_eq!(NetEndpoint::tcp(addr).protocol, TransportProtocol::Tcp);
        assert_eq!(NetEndpoint::udp(addr).addr, addr);
    }

    #[test]
    fn endpoints_differing_only_in_protocol_are_distinct() {
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 30490));
        assert_ne!(NetEndpoint::udp(addr), NetEndpoint::tcp(addr));
    }
}
