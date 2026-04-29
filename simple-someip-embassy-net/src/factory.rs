//! `TransportFactory` impl over embassy-net's UDP API.
//!
//! See the crate-level doc for context. This module is the meat of the
//! adapter: a fixed-capacity pool of UDP-socket buffers backing a
//! `TransportFactory` whose `bind()` hands out one slot per call and
//! reclaims it when the returned [`EmbassyNetSocket`] is dropped.

use core::cell::UnsafeCell;
use core::future::Future;
use core::net::SocketAddrV4;
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_net::Stack;
use embassy_net::driver::Driver;
use embassy_net::udp::{PacketMetadata, UdpSocket};

use simple_someip::transport::{IoErrorKind, SocketOptions, TransportError, TransportFactory};

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

// SAFETY: the `slots` field is accessed only via the per-slot
// `in_use` AtomicBool: a slot's UnsafeCell-wrapped storage is
// touched only between a successful CAS `false -> true` and the
// reciprocal `true -> false` on slot release. Cross-task access is
// serialized by that CAS handshake, which gives us the same
// happens-before guarantees as a Mutex<T> would.
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
pub struct EmbassyNetFactory<'pool, D, const POOL: usize, const RX_BUF: usize, const TX_BUF: usize>
where
    D: Driver + 'static,
{
    stack: &'static Stack<D>,
    pool: &'pool SocketPool<POOL, RX_BUF, TX_BUF>,
}

impl<'pool, D, const POOL: usize, const RX_BUF: usize, const TX_BUF: usize>
    EmbassyNetFactory<'pool, D, POOL, RX_BUF, TX_BUF>
where
    D: Driver + 'static,
{
    /// Build a factory borrowing from the given `Stack` and socket pool.
    ///
    /// The `Stack` reference must be `'static` because each bound
    /// [`UdpSocket`] borrows from it for the socket's lifetime, and
    /// our [`EmbassyNetSocket`] is stored in the simple-someip
    /// run-loop's task state (which itself outlives the
    /// `EmbassyNetFactory`).
    #[must_use]
    pub fn new(stack: &'static Stack<D>, pool: &'pool SocketPool<POOL, RX_BUF, TX_BUF>) -> Self {
        Self { stack, pool }
    }
}

/// Named future for the synchronous `bind` step.
///
/// `EmbassyNetFactory::bind` is logically synchronous — claim a
/// pool slot, construct the `UdpSocket`, call `bind(port)` — but
/// the trait wants a `Future`. This wrapper resolves on the first
/// poll. The `Option`-and-take pattern lets us yield the eventual
/// `Result` exactly once per future without storing it twice.
pub struct EmbassyNetBindFuture {
    inner: Option<Result<EmbassyNetSocket, TransportError>>,
}

impl Future for EmbassyNetBindFuture {
    type Output = Result<EmbassyNetSocket, TransportError>;

    fn poll(
        mut self: core::pin::Pin<&mut Self>,
        _cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Self::Output> {
        match self.inner.take() {
            Some(result) => core::task::Poll::Ready(result),
            None => panic!("EmbassyNetBindFuture polled after completion"),
        }
    }
}

impl<D, const POOL: usize, const RX_BUF: usize, const TX_BUF: usize> TransportFactory
    for EmbassyNetFactory<'static, D, POOL, RX_BUF, TX_BUF>
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
                inner: Some(Err(TransportError::AddressInUse)),
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
        // Lifetime erasure: UnsafeCell::get() returns *mut T; we
        // dereference to &'static mut [T]. That's sound because
        // (a) the SocketPool itself is &'static (held by the
        // factory as &'pool, but the pool we pass at construction
        // is required to be &'static for the F::Socket: 'static
        // bound elsewhere — see the impl bound above) and (b) the
        // exclusive-access invariant from in_use serializes
        // overlapping mutations.
        let (rx_meta, rx_buf, tx_meta, tx_buf) = unsafe {
            (
                &mut *slot.rx_meta.get(),
                &mut *slot.rx_buf.get(),
                &mut *slot.tx_meta.get(),
                &mut *slot.tx_buf.get(),
            )
        };

        let mut socket = UdpSocket::new(self.stack, rx_meta, rx_buf, tx_meta, tx_buf);

        // 3. bind() to the requested port. Port 0 means
        //    "ephemeral, let the stack pick" — embassy-net
        //    interprets bind on a `port: 0` IpListenEndpoint as
        //    "any port". The actual local addr is read back via
        //    EmbassyNetSocket::local_addr.
        if let Err(_e) = socket.bind(addr.port()) {
            // Bind failed. Release the slot so it doesn't leak.
            // SAFETY: slot was claimed at the top of this fn; no
            // other path has observed it.
            self.pool.in_use[slot_index].store(false, Ordering::Release);
            return EmbassyNetBindFuture {
                inner: Some(Err(TransportError::AddressInUse)),
            };
        }

        // 4. Wrap into our EmbassyNetSocket. Erase the pool's
        //    const generics by coercing &'static SocketPool<...>
        //    to &'static dyn SlotReclaim — the socket only ever
        //    needs to call `release(slot_index)` on drop.
        //
        // SAFETY: see the lifetime-erasure note above.
        let pool_dyn: &'static dyn SlotReclaim = unsafe {
            // Lift `self.pool: &SocketPool<...>` from `'pool` to
            // `'static`. The `impl<...> for EmbassyNetFactory<'static, ...>`
            // bound above guarantees the factory we're being called
            // through has a `'static` pool reference, so the lift
            // is identity.
            core::mem::transmute::<
                &SocketPool<POOL, RX_BUF, TX_BUF>,
                &'static SocketPool<POOL, RX_BUF, TX_BUF>,
            >(self.pool)
        };
        let local = SocketAddrV4::new(*addr.ip(), addr.port());
        let socket = EmbassyNetSocket::new(socket, local, slot_index, pool_dyn);

        EmbassyNetBindFuture {
            inner: Some(Ok(socket)),
        }
    }
}

/// Internal: unused-import guard so `IoErrorKind` stays threaded
/// through for use in the upcoming 19c socket-level error mapping.
#[allow(dead_code)]
fn _phantom_io_error_kind_use() -> IoErrorKind {
    IoErrorKind::Other
}
