//! `TransportFactory` impl over embassy-net's UDP API.
//!
//! See the crate-level doc for context. This module is the meat of the
//! adapter: a fixed-capacity pool of UDP-socket buffers backing a
//! `TransportFactory` whose `bind()` hands out one slot per call and
//! reclaims it when the returned [`EmbassyNetSocket`] is dropped.

use core::cell::UnsafeCell;
use core::future::Ready;
use core::marker::PhantomData;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_net::Stack;
use embassy_net::driver::Driver;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpAddress, IpListenEndpoint};

use simple_someip::transport::{SocketOptions, TransportError, TransportFactory};

use crate::socket::{EmbassyNetSocket, SlotReclaim};

/// `PacketMetadata` entries per direction per socket.
///
/// embassy-net needs this for its smoltcp-backed UDP slot bookkeeping
/// (one entry per buffered datagram). 4 is enough headroom for the
/// SOME/IP-SD workload (announcement tick + occasional Subscribe);
/// firmware with more bursty receive patterns may need to raise it.
/// Hard-coded rather than const-generic because (a) it's never the
/// real sizing knob and (b) extra const generics on the public
/// surface make the type signatures actively annoying.
pub const PACKET_METADATA_LEN: usize = 4;

/// Caller-owned pool of UDP-socket buffer storage.
///
/// embassy-net's [`UdpSocket::new`] requires the caller to provide
/// `&mut` references to RX/TX byte buffers and per-direction
/// [`PacketMetadata`] arrays. The socket borrows them for its
/// lifetime.
///
/// To satisfy `simple-someip`'s `F::Socket: 'static` bound (the
/// run-loop spawns per-socket I/O tasks), the buffers must live in
/// `&'static` storage. `SocketPool` declares `POOL` slots of buffer
/// storage in a single `static` and the [`EmbassyNetFactory`] hands
/// each `bind()` call a fresh slot.
///
/// # Buffer sizing — IMPORTANT
///
/// `RX_BUF` / `TX_BUF` are **link-layer payload caps**, not application
/// payload caps. SOME/IP-over-UDP datagrams are bounded by the
/// path-MTU minus the IP header (20 B for IPv4) minus the UDP header
/// (8 B). For a 1500-byte Ethernet MTU that's a 1472-byte ceiling on
/// the application payload before fragmentation. Sizing
/// `RX_BUF`/`TX_BUF` to **at least** the link MTU (1500) gives full
/// headroom for any datagram the L2/L3 stack will deliver; sizing
/// strictly to the application cap (1472) risks dropping otherwise-
/// valid datagrams. Most consumers should pick 1500 or larger.
///
/// # Example
///
/// ```ignore
/// use simple_someip_embassy_net::{EmbassyNetFactory, SocketPool};
///
/// // 4 sockets, each with 1500-byte RX/TX buffers (matches
/// // simple-someip's UDP_BUFFER_SIZE).
/// static POOL: SocketPool<4, 1500, 1500> = SocketPool::new();
///
/// let factory = EmbassyNetFactory::new(stack, &POOL);
/// ```
///
/// # Capacity sizing
///
/// One slot per simultaneously-bound UDP socket. The simple-someip
/// `Client` needs one for the discovery socket plus up to
/// `UNICAST_SOCKETS_CAP = 8` for unicast endpoints (see
/// `simple-someip`'s docs). Sizing `POOL` to 9-10 covers a single
/// `Client`; add more for multiple `Client` instances or a
/// concurrent `Server`.
pub struct SocketPool<const POOL: usize, const RX_BUF: usize, const TX_BUF: usize> {
    slots: [Slot<RX_BUF, TX_BUF>; POOL],
    in_use: [AtomicBool; POOL],
}

// SAFETY: `SocketPool::Sync` is sound for shared *slot data* access:
// each slot's `UnsafeCell`-wrapped storage is touched only between a
// successful CAS `false -> true` (in `claim`) and the reciprocal
// `true -> false` on release (in `Drop`). That CAS handshake gives
// the same happens-before guarantee as a `Mutex`. NOTE: this only
// covers the *pool*'s slot data — the `EmbassyNetFactory` that
// mediates `bind()` is intentionally `!Send + !Sync` (see
// `_not_thread_safe: PhantomData<*const ()>` below) because
// `embassy_net::Stack` uses interior `RefCell` and is not safe to
// drive `bind()` on from multiple threads.
unsafe impl<const POOL: usize, const RX_BUF: usize, const TX_BUF: usize> Sync
    for SocketPool<POOL, RX_BUF, TX_BUF>
{
}

struct Slot<const RX_BUF: usize, const TX_BUF: usize> {
    rx_meta: UnsafeCell<[PacketMetadata; PACKET_METADATA_LEN]>,
    rx_buf: UnsafeCell<[u8; RX_BUF]>,
    tx_meta: UnsafeCell<[PacketMetadata; PACKET_METADATA_LEN]>,
    tx_buf: UnsafeCell<[u8; TX_BUF]>,
}

impl<const RX_BUF: usize, const TX_BUF: usize> Slot<RX_BUF, TX_BUF> {
    const fn new() -> Self {
        Self {
            rx_meta: UnsafeCell::new([PacketMetadata::EMPTY; PACKET_METADATA_LEN]),
            rx_buf: UnsafeCell::new([0u8; RX_BUF]),
            tx_meta: UnsafeCell::new([PacketMetadata::EMPTY; PACKET_METADATA_LEN]),
            tx_buf: UnsafeCell::new([0u8; TX_BUF]),
        }
    }
}

impl<const POOL: usize, const RX_BUF: usize, const TX_BUF: usize> SocketPool<POOL, RX_BUF, TX_BUF> {
    /// Construct an empty socket pool. `const`, so the pool can live
    /// in a plain `static` declaration in firmware boot code.
    #[must_use]
    pub const fn new() -> Self {
        // `[const { ... }; N]` lets us const-init both arrays
        // without spelling out N copies.
        Self {
            slots: [const { Slot::new() }; POOL],
            in_use: [const { AtomicBool::new(false) }; POOL],
        }
    }

    /// Try to claim a free slot. Returns the slot index on success.
    fn claim(&self) -> Option<usize> {
        for (i, flag) in self.in_use.iter().enumerate() {
            if flag
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(i);
            }
        }
        None
    }
}

impl<const POOL: usize, const RX_BUF: usize, const TX_BUF: usize> Default
    for SocketPool<POOL, RX_BUF, TX_BUF>
{
    fn default() -> Self {
        Self::new()
    }
}

// `SlotReclaim` is the dynless free-list-release hook handed to
// `EmbassyNetSocket`. Each pool implements it; the socket carries a
// `&'static dyn SlotReclaim`-style pointer so the socket type
// itself doesn't carry the pool's `POOL` / `RX_BUF` / `TX_BUF`
// const generics.
impl<const POOL: usize, const RX_BUF: usize, const TX_BUF: usize> SlotReclaim
    for SocketPool<POOL, RX_BUF, TX_BUF>
{
    fn release(&self, slot_index: usize) {
        // `Release` ordering pairs with the `Acquire` on the next
        // `claim()`, ensuring writes the previous owner did to the
        // slot's UnsafeCell-wrapped storage are visible to the
        // next claimant.
        self.in_use[slot_index].store(false, Ordering::Release);
    }
}

/// embassy-net `TransportFactory` implementation.
///
/// Holds a reference to the embassy-net `Stack<D>` and a `&'static`
/// [`SocketPool`] from which `bind()` allocates per-socket buffers.
///
/// # Thread-safety
///
/// `EmbassyNetFactory` is intentionally `!Send + !Sync`. embassy-net's
/// `Stack<D>` uses interior `RefCell` for its socket-set bookkeeping
/// and is designed to be driven from a single embassy executor task;
/// allowing the factory to cross thread boundaries would let two
/// threads call `bind()` concurrently and race on the stack's
/// `borrow_mut()`. The simple-someip run-loops live on one task per
/// `Client` / `Server` anyway, which matches this constraint.
///
/// # Multicast group join (important)
///
/// `TransportSocket::join_multicast_v4` on the returned socket is
/// **a documented no-op** because embassy-net's multicast-group
/// join lives on [`Stack::join_multicast_group`] and is async,
/// while our trait method is sync. The user is expected to call
/// `stack.join_multicast_group(...)` at stack-init time, BEFORE
/// constructing the `Client` — typically:
///
/// ```ignore
/// // At stack init:
/// stack.join_multicast_group(simple_someip::protocol::sd::MULTICAST_IP)
///     .await
///     .unwrap();
///
/// // Then build the Client:
/// let factory = EmbassyNetFactory::new(stack, &POOL);
/// let (client, ..) = Client::new_with_deps(...);
/// ```
///
/// Without that explicit join, multicast SD traffic will not be
/// delivered to any socket bound through this factory.
pub struct EmbassyNetFactory<D, const POOL: usize, const RX_BUF: usize, const TX_BUF: usize>
where
    D: Driver + 'static,
{
    stack: &'static Stack<D>,
    pool: &'static SocketPool<POOL, RX_BUF, TX_BUF>,
    /// Marker that pins the factory to a single thread. embassy-net's
    /// `Stack` is not safe to drive `bind()` on from multiple threads
    /// because of its internal `RefCell`. `*const ()` makes us
    /// `!Send + !Sync` without occupying any storage.
    _not_thread_safe: PhantomData<*const ()>,
}

impl<D, const POOL: usize, const RX_BUF: usize, const TX_BUF: usize>
    EmbassyNetFactory<D, POOL, RX_BUF, TX_BUF>
where
    D: Driver + 'static,
{
    /// Build a factory borrowing from the given `Stack` and socket pool.
    ///
    /// Both references must be `'static` because each bound
    /// [`UdpSocket`] borrows from the stack and pool storage for the
    /// socket's lifetime, and our [`EmbassyNetSocket`] is stored in
    /// the simple-someip run-loop's task state (which itself outlives
    /// the `EmbassyNetFactory`).
    #[must_use]
    pub fn new(
        stack: &'static Stack<D>,
        pool: &'static SocketPool<POOL, RX_BUF, TX_BUF>,
    ) -> Self {
        Self {
            stack,
            pool,
            _not_thread_safe: PhantomData,
        }
    }
}

/// Named future for the synchronous `bind` step.
///
/// `EmbassyNetFactory::bind` is logically synchronous — claim a
/// pool slot, construct the `UdpSocket`, call `bind(port)` — but
/// the trait wants a `Future`. We delegate to [`core::future::Ready`]
/// so the future resolves on first poll. Polling after completion
/// panics with `core::future::Ready`'s standard message ("`Ready`
/// polled after completion") — a Future-contract violation by the
/// caller; not something a well-behaved executor will trigger.
pub struct EmbassyNetBindFuture {
    inner: Ready<Result<EmbassyNetSocket, TransportError>>,
}

impl core::future::Future for EmbassyNetBindFuture {
    type Output = Result<EmbassyNetSocket, TransportError>;

    fn poll(
        self: core::pin::Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Self::Output> {
        // Project the inner Ready and forward poll. We're a
        // structural Pin destination per pin-projection rules: the
        // inner `Ready` is itself `Unpin`, so we can take a `&mut`
        // through the `Pin<&mut Self>` projection safely.
        let me = unsafe { self.get_unchecked_mut() };
        core::pin::Pin::new(&mut me.inner).poll(cx)
    }
}

impl<D, const POOL: usize, const RX_BUF: usize, const TX_BUF: usize> TransportFactory
    for EmbassyNetFactory<D, POOL, RX_BUF, TX_BUF>
where
    D: Driver + 'static,
{
    type Socket = EmbassyNetSocket;
    type BindFuture<'a> = EmbassyNetBindFuture;

    fn bind<'a>(&'a self, addr: SocketAddrV4, _options: &'a SocketOptions) -> Self::BindFuture<'a> {
        // 1. Claim a free slot. If none, return `AddressInUse` —
        //    the closest existing variant; a future TransportError
        //    addition could carry a dedicated `PoolExhausted` kind.
        let Some(slot_index) = self.pool.claim() else {
            return EmbassyNetBindFuture {
                inner: core::future::ready(Err(TransportError::AddressInUse)),
            };
        };

        let slot = &self.pool.slots[slot_index];

        // 2. Build the UdpSocket borrowing from the slot's
        //    UnsafeCell-wrapped storage.
        //
        // SAFETY: the slot is now claimed (we just CAS'd in_use
        // false → true). No other code path will read/write this
        // slot's UnsafeCells while in_use is true. The borrows we
        // take here are valid until the corresponding
        // EmbassyNetSocket is dropped, at which point in_use is
        // set back to false (in `socket::Drop`); the next claim()
        // observes that via Acquire.
        //
        // Lifetime: `self.pool` is already `&'static`, so the
        // `&mut` reborrows below are `'static` too. No transmute
        // needed.
        let (rx_meta, rx_buf, tx_meta, tx_buf) = unsafe {
            (
                &mut *slot.rx_meta.get(),
                &mut *slot.rx_buf.get(),
                &mut *slot.tx_meta.get(),
                &mut *slot.tx_buf.get(),
            )
        };

        let mut socket = UdpSocket::new(self.stack, rx_meta, rx_buf, tx_meta, tx_buf);

        // 3. bind() to the requested endpoint.
        //
        // Honor `addr.ip()`: if the caller specified a non-wildcard
        // local address, bind to it (otherwise smoltcp would accept
        // datagrams on any interface, ignoring caller intent). For
        // `0.0.0.0` we pass `addr: None` so embassy-net binds on
        // any local interface (its "wildcard" mode).
        //
        // Port 0 means "ephemeral, let the stack pick" — embassy-net
        // allocates a dynamic port and writes it back into the
        // bound endpoint, which we read out via `socket.endpoint()`
        // below to record the actual local address.
        let listen_addr: Option<IpAddress> = if addr.ip().is_unspecified() {
            None
        } else {
            let o = addr.ip().octets();
            Some(IpAddress::v4(o[0], o[1], o[2], o[3]))
        };
        let listen_endpoint = IpListenEndpoint {
            addr: listen_addr,
            port: addr.port(),
        };
        if socket.bind(listen_endpoint).is_err() {
            // Bind failed. Release the slot so it doesn't leak.
            // SAFETY: slot was claimed at the top of this fn; no
            // other path has observed it.
            self.pool.release(slot_index);
            return EmbassyNetBindFuture {
                inner: core::future::ready(Err(TransportError::AddressInUse)),
            };
        }

        // 4. Read back the actual bound port. embassy-net replaces
        //    `port: 0` with the picked ephemeral port inside
        //    `bind()`, so `endpoint().port` is the truth post-bind.
        //    The address we record is what the caller asked for
        //    (with `0.0.0.0` preserved as the wildcard) — embassy-
        //    net's `endpoint().addr` is `None` for wildcard binds
        //    and we have nothing better to substitute there.
        let actual_port = socket.endpoint().port;
        let local = SocketAddrV4::new(*addr.ip(), actual_port);

        // 5. Wrap into our EmbassyNetSocket. `&'static SocketPool`
        //    coerces directly to `&'static dyn SlotReclaim`; no
        //    transmute / lifetime erasure needed.
        let pool_dyn: &'static dyn SlotReclaim = self.pool;
        let socket = EmbassyNetSocket::new(socket, local, slot_index, pool_dyn);

        EmbassyNetBindFuture {
            inner: core::future::ready(Ok(socket)),
        }
    }
}

// Compile-time assertion documented at the type level: `Ipv4Addr`
// `is_unspecified()` returns true exactly when the address is
// `0.0.0.0`. This keeps a future Rust stdlib reshape from silently
// changing how `bind` interprets the wildcard IP.
const _: () = {
    assert!(Ipv4Addr::UNSPECIFIED.is_unspecified());
};
