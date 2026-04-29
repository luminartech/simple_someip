//! `TransportSocket` impl wrapping `embassy_net::udp::UdpSocket`.
//!
//! Phase 19a scaffold; full impl in 19c.

/// embassy-net-backed [`simple_someip::transport::TransportSocket`].
///
/// Holds an `embassy_net::udp::UdpSocket<'a>` borrowing into
/// caller-owned `&'static` buffer storage (managed by
/// [`crate::SocketPool`] / [`crate::EmbassyNetFactory`]).
///
/// **NB phase 19a:** the [`TransportSocket`](simple_someip::transport::TransportSocket)
/// trait impl lands in 19c. This skeleton lets [`crate::factory`]
/// reference the type without forward-declaration gymnastics.
#[allow(dead_code)] // populated in 19c
pub struct EmbassyNetSocket {
    // Inner `UdpSocket<'a>` + bookkeeping (pool slot index for
    // free-list reclamation, local addr) lands in 19c.
    _todo: (),
}
