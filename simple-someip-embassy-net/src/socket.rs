//! `TransportSocket` impl wrapping `embassy_net::udp::UdpSocket`.
//!
//! Phase 19b: the type is constructed by [`crate::factory::EmbassyNetFactory::bind`]
//! and carries the slot-reclamation hook so its `Drop` impl returns
//! the buffer pool slot to the free list. The `TransportSocket`
//! trait impl (named `send_to` / `recv_from` futures driving
//! `poll_send_to` / `poll_recv_from`, plus the multicast / local-addr
//! shims) lands in 19c.

use core::future::Ready;
use core::net::{Ipv4Addr, SocketAddrV4};

use embassy_net::udp::UdpSocket;
use simple_someip::transport::{ReceivedDatagram, TransportError, TransportSocket};

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

    /// Borrow the inner `UdpSocket` for the upcoming
    /// `TransportSocket` send/recv impl in 19c.
    #[allow(dead_code)] // wired in 19c
    pub(crate) fn inner(&self) -> &UdpSocket<'static> {
        &self.inner
    }

    /// Local address recorded at bind time.
    #[allow(dead_code)] // wired in 19c
    pub(crate) fn local(&self) -> SocketAddrV4 {
        self.local
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

// ── TransportSocket impl (stub) ──────────────────────────────────────
//
// Phase 19b ships a minimum-viable impl so the
// `EmbassyNetFactory::TransportFactory` impl typechecks (the trait
// requires `Self::Socket: TransportSocket`). Every method here
// returns `Err(TransportError::Unsupported)`. Phase 19c replaces
// them with real `poll_send_to` / `poll_recv_from`-driven named
// futures.
//
// Until 19c, attempting to use a bound `EmbassyNetSocket` for actual
// I/O will fail at runtime with `Unsupported`. This is intentional:
// the 19b commit verifies the factory + pool + Drop wiring without
// requiring the full I/O bring-up, which is its own scoped work.

impl TransportSocket for EmbassyNetSocket {
    type SendFuture<'a> = Ready<Result<(), TransportError>>;
    type RecvFuture<'a> = Ready<Result<ReceivedDatagram, TransportError>>;

    fn send_to<'a>(&'a self, _buf: &'a [u8], _target: SocketAddrV4) -> Self::SendFuture<'a> {
        // 19c: drive `inner.poll_send_to(buf, target.into(), cx)`.
        core::future::ready(Err(TransportError::Unsupported))
    }

    fn recv_from<'a>(&'a self, _buf: &'a mut [u8]) -> Self::RecvFuture<'a> {
        // 19c: drive `inner.poll_recv_from(buf, cx)`.
        core::future::ready(Err(TransportError::Unsupported))
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
