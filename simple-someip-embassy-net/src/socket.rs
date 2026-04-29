//! `TransportSocket` impl wrapping `embassy_net::udp::UdpSocket`.
//!
//! Phase 19c lands the real send/recv I/O — named future structs
//! drive `embassy_net`'s `poll_send_to` / `poll_recv_from` directly,
//! so each datagram costs zero heap allocations on the hot path.

use core::future::Future;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::pin::Pin;
use core::task::{Context, Poll};

use embassy_net::udp::{RecvError, SendError, UdpSocket};
use embassy_net::{IpAddress, IpEndpoint};

use simple_someip::transport::{IoErrorKind, ReceivedDatagram, TransportError, TransportSocket};

/// Hook implemented by [`crate::SocketPool`] for releasing a
/// claimed slot back to the free list when an
/// [`EmbassyNetSocket`] is dropped. Type-erased via
/// `&'static dyn SlotReclaim` so that [`EmbassyNetSocket`] does not
/// carry the pool's `POOL` / `RX_BUF` / `TX_BUF` const generics on
/// its own type signature.
pub trait SlotReclaim: Sync {
    /// Release slot `slot_index` back to the free list.
    fn release(&self, slot_index: usize);
}

/// embassy-net-backed [`simple_someip::transport::TransportSocket`].
///
/// Holds an `embassy_net::udp::UdpSocket<'static>` borrowing into
/// caller-owned `&'static` buffer storage (managed by
/// [`crate::SocketPool`] / [`crate::EmbassyNetFactory`]). The
/// `'static` lifetime is materialised inside
/// [`crate::EmbassyNetFactory::bind`] via `UnsafeCell` projection
/// over a `&'static SocketPool` — see the SAFETY comment there.
///
/// On drop, returns its pool slot to the free list so a subsequent
/// `bind()` call can reuse the buffers.
pub struct EmbassyNetSocket {
    inner: UdpSocket<'static>,
    /// Local address reported by [`Self::local_addr`]. Recorded at
    /// `bind()` time; embassy-net's `endpoint()` returns an
    /// `IpListenEndpoint` whose `addr` is `None` for "any
    /// interface" binds, so we keep the user's intent here
    /// instead.
    local: SocketAddrV4,
    slot_index: usize,
    reclaim: &'static dyn SlotReclaim,
}

impl EmbassyNetSocket {
    /// Construct from the parts the factory just claimed. Crate-private.
    pub(crate) fn new(
        inner: UdpSocket<'static>,
        local: SocketAddrV4,
        slot_index: usize,
        reclaim: &'static dyn SlotReclaim,
    ) -> Self {
        Self {
            inner,
            local,
            slot_index,
            reclaim,
        }
    }
}

impl Drop for EmbassyNetSocket {
    fn drop(&mut self) {
        // Close the underlying socket explicitly first — embassy-net
        // releases its smoltcp slot here and stops accepting traffic.
        // Then release our pool slot so the buffers can be reused.
        self.inner.close();
        self.reclaim.release(self.slot_index);
    }
}

// ── Named send / recv futures ────────────────────────────────────────
//
// Hand-rolled `Future` types over embassy-net's `poll_send_to` /
// `poll_recv_from` rather than wrapping the async `send_to` /
// `recv_from` in `Box::pin(async move { ... })`. The named-struct
// shape is what makes the adapter zero-alloc on the hot path —
// every datagram incurs no allocator traffic.

/// Future returned by [`EmbassyNetSocket::send_to`]. Drives
/// `embassy_net::udp::UdpSocket::poll_send_to` directly.
pub struct EmbassyNetSendFut<'a> {
    socket: &'a UdpSocket<'static>,
    buf: &'a [u8],
    target: IpEndpoint,
}

impl Future for EmbassyNetSendFut<'_> {
    type Output = Result<(), TransportError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // EmbassyNetSendFut has no self-referential fields; the
        // underlying `UdpSocket::poll_send_to` only borrows
        // through `&self`, and `me.buf` is a fresh reborrow every
        // poll. Safe to project to `&mut Self`.
        let me = self.get_mut();
        match me.socket.poll_send_to(me.buf, me.target, cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(SendError::NoRoute)) => {
                Poll::Ready(Err(TransportError::Io(IoErrorKind::NetworkUnreachable)))
            }
            Poll::Ready(Err(SendError::SocketNotBound)) => {
                // Programming error — we always bind before
                // returning the socket from `EmbassyNetFactory::bind`.
                // Surface as `Other` so it shows up in operator
                // logs distinctly from a routing failure.
                Poll::Ready(Err(TransportError::Io(IoErrorKind::Other)))
            }
        }
    }
}

/// Future returned by [`EmbassyNetSocket::recv_from`]. Drives
/// `embassy_net::udp::UdpSocket::poll_recv_from` directly.
pub struct EmbassyNetRecvFut<'a> {
    socket: &'a UdpSocket<'static>,
    buf: &'a mut [u8],
}

impl Future for EmbassyNetRecvFut<'_> {
    type Output = Result<ReceivedDatagram, TransportError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = self.get_mut();
        match me.socket.poll_recv_from(me.buf, cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok((n, endpoint))) => match endpoint_to_socket_addr_v4(endpoint) {
                Some(source) => Poll::Ready(Ok(ReceivedDatagram {
                    bytes_received: n,
                    source,
                    // embassy-net's `recv_slice` returns
                    // `Truncated` (mapped to `Err` below) when the
                    // datagram doesn't fit; on the success path it
                    // delivered the whole thing.
                    truncated: false,
                })),
                None => {
                    // IPv6 source on a v4-bound SOME/IP socket is a
                    // misconfiguration upstream — surface as
                    // `Unsupported` for the same reason
                    // `tokio_transport::recv_from` does.
                    Poll::Ready(Err(TransportError::Unsupported))
                }
            },
            Poll::Ready(Err(RecvError::Truncated)) => {
                // Caller's buffer was smaller than the datagram.
                // simple-someip uses `UDP_BUFFER_SIZE = 1500` for
                // its recv buffers, which exceeds typical UDP
                // payloads — hitting this branch indicates either
                // an undersized SocketPool RX_BUF or an
                // unexpectedly large incoming datagram. Either way
                // the application has a sizing problem worth
                // logging through the operator pipeline.
                Poll::Ready(Err(TransportError::Io(IoErrorKind::Other)))
            }
        }
    }
}

impl TransportSocket for EmbassyNetSocket {
    type SendFuture<'a> = EmbassyNetSendFut<'a>;
    type RecvFuture<'a> = EmbassyNetRecvFut<'a>;

    fn send_to<'a>(&'a self, buf: &'a [u8], target: SocketAddrV4) -> Self::SendFuture<'a> {
        EmbassyNetSendFut {
            socket: &self.inner,
            buf,
            target: socket_addr_v4_to_endpoint(target),
        }
    }

    fn recv_from<'a>(&'a self, buf: &'a mut [u8]) -> Self::RecvFuture<'a> {
        EmbassyNetRecvFut {
            socket: &self.inner,
            buf,
        }
    }

    fn local_addr(&self) -> Result<SocketAddrV4, TransportError> {
        Ok(self.local)
    }

    fn join_multicast_v4(&self, _group: Ipv4Addr, _iface: Ipv4Addr) -> Result<(), TransportError> {
        // embassy-net's multicast-group join lives on
        // `Stack::join_multicast_group` and is async; the user is
        // expected to have called it BEFORE constructing any
        // EmbassyNetSocket (see EmbassyNetFactory's docstring). We
        // return Ok(()) here so simple-someip's `bind_discovery`
        // path (which always tries to join) does not error out;
        // the real multicast subscription has to have happened on
        // the stack already.
        Ok(())
    }

    fn leave_multicast_v4(&self, _group: Ipv4Addr, _iface: Ipv4Addr) -> Result<(), TransportError> {
        // Symmetric to join_multicast_v4 — leave is also on the
        // stack, not the socket. Documented no-op.
        Ok(())
    }
}

// ── Address conversions ──────────────────────────────────────────────

fn socket_addr_v4_to_endpoint(addr: SocketAddrV4) -> IpEndpoint {
    let o = addr.ip().octets();
    IpEndpoint {
        addr: IpAddress::v4(o[0], o[1], o[2], o[3]),
        port: addr.port(),
    }
}

/// Convert an embassy-net `IpEndpoint` to `SocketAddrV4`. Returns
/// `None` for non-IPv4 endpoints (SOME/IP's transport layer is
/// IPv4-only at this layer; an IPv6 source on a v4-bound socket
/// indicates a misconfiguration upstream).
///
/// The wildcard arm covers the case where smoltcp's `proto-ipv6`
/// feature gets pulled in via cargo's feature unification (e.g.
/// another crate in the dep graph enables it). Without the arm
/// the match would silently become non-exhaustive in that build.
fn endpoint_to_socket_addr_v4(endpoint: IpEndpoint) -> Option<SocketAddrV4> {
    match endpoint.addr {
        IpAddress::Ipv4(v4) => {
            // smoltcp's `Ipv4Address` is `pub struct Address(pub [u8; 4])`
            // — no `octets()` accessor; the public tuple field is the
            // documented way in.
            let o = v4.0;
            Some(SocketAddrV4::new(
                Ipv4Addr::new(o[0], o[1], o[2], o[3]),
                endpoint.port,
            ))
        }
        #[allow(unreachable_patterns)]
        _ => None,
    }
}
