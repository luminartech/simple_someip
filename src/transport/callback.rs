//! Callback-driven `no_std` transport for embedded firmware.
//!
//! Implements [`AsyncUdpSocket`], [`Clock`], and [`SocketFactory`]
//! on top of a four-function callback contract that a host
//! integration provides via a type implementing
//! [`TransportCallbacks`]. The callbacks typically map onto a C
//! network stack (lwIP, embedded-net, etc.) that owns the actual
//! UDP PCBs and clock; the Rust side handles SOME/IP state-machine
//! work and uses the callbacks to push bytes through the host.
//!
//! ## Wiring
//!
//! 1. Define a zero-sized marker type and implement
//!    [`TransportCallbacks`] for it.
//! 2. Instantiate the SOME/IP client/server with
//!    `CallbackUdpSocket<MyCallbacks>` /
//!    `CallbackClock<MyCallbacks>` /
//!    `CallbackSocketFactory<MyCallbacks>`.
//! 3. From the host C side, call
//!    [`simple_someip_callback_transport_on_rx`] for every received
//!    UDP datagram on a port the factory bound.
//!
//! ## Single-instance constraint
//!
//! Only one `TransportCallbacks` impl per binary is supported — the
//! per-port RX slot state is a single `static`. This matches typical
//! embedded use where one process integrates one SOME/IP stack.

use core::cell::UnsafeCell;
use core::cmp::Ordering as CmpOrdering;
use core::future::Future;
use core::marker::PhantomData;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::ops::Add;
use core::pin::Pin;
use core::task::{Context, Poll};
use core::time::Duration;

use crate::runtime::{AsyncUdpSocket, Clock, SocketFactory};

// ===========================================================================
// Configuration constants
// ===========================================================================

/// Largest UDP datagram the transport will accept. Sized for typical
/// SD + small-payload notifications; large request payloads (e.g.
/// iris HWP1ScanCommand at ~10.5 KiB) exceed this and are dropped
/// at the on_rx boundary. Bump per-deployment as the static memory
/// budget allows.
pub const MAX_DATAGRAM: usize = 2048;

/// Number of distinct local ports the transport can serve concurrently.
pub const MAX_SOCKETS: usize = 3;

// ===========================================================================
// Error type
// ===========================================================================

#[derive(Debug, Clone, Copy)]
pub enum CallbackTransportError {
    /// `bind_unicast` / `bind_discovery` returned an error.
    BindFailed,
    /// `send_udp` returned a non-zero error code.
    SendFailed,
    /// All [`MAX_SOCKETS`] slots are already in use.
    SlotPoolExhausted,
    /// `poll_recv_from` was called with a buffer smaller than the
    /// pending datagram.
    OutputBufferTooSmall,
}

// ===========================================================================
// Host callbacks trait
// ===========================================================================

/// Host integration contract.
///
/// The four trait methods bridge the runtime traits to a C transport.
/// Implementations are typically zero-sized marker types whose
/// methods are simple `unsafe extern` calls.
///
/// All addresses and ports are in **host byte order**.
pub trait TransportCallbacks {
    /// Send `buf` as a UDP datagram from `local_port` to
    /// `dst_addr:dst_port`. Returns 0 on success, non-zero on
    /// error.
    fn send_udp(local_port: u16, buf: &[u8], dst_addr: u32, dst_port: u16) -> i32;

    /// Read the monotonic millisecond tick. Wrap is handled by
    /// [`WrappingMs`].
    fn now_ms() -> u32;

    /// Bind a unicast UDP socket on `port` (0 = ephemeral). Returns
    /// the actually-bound port on success, 0 on error.
    fn bind_unicast(port: u16) -> u16;

    /// Bind the SOME/IP-SD multicast socket and join the SD group.
    /// `multicast_loopback` controls whether self-sent multicasts
    /// loop back on the same host. Returns 0 on success, non-zero
    /// on error.
    fn bind_discovery(multicast_loopback: bool) -> i32;
}

// ===========================================================================
// Per-port RX slot state (process-wide singleton)
// ===========================================================================

struct PendingDatagram {
    src_addr: u32,
    src_port: u16,
    len: u16,
    bytes: [u8; MAX_DATAGRAM],
}

impl PendingDatagram {
    const fn empty() -> Self {
        Self {
            src_addr: 0,
            src_port: 0,
            len: 0,
            bytes: [0; MAX_DATAGRAM],
        }
    }
}

struct PortSlot {
    /// `0` means the slot is free.
    local_port: u16,
    has_pending: bool,
    pending: PendingDatagram,
}

impl PortSlot {
    const fn empty() -> Self {
        Self {
            local_port: 0,
            has_pending: false,
            pending: PendingDatagram::empty(),
        }
    }
}

#[repr(transparent)]
struct SyncCell<T>(UnsafeCell<T>);
unsafe impl<T> Sync for SyncCell<T> {}

static SLOTS: SyncCell<[PortSlot; MAX_SOCKETS]> = SyncCell(UnsafeCell::new([
    PortSlot::empty(),
    PortSlot::empty(),
    PortSlot::empty(),
]));

#[inline]
unsafe fn slots_mut() -> &'static mut [PortSlot; MAX_SOCKETS] {
    unsafe { &mut *SLOTS.0.get() }
}

fn allocate_slot(local_port: u16) -> Result<(), CallbackTransportError> {
    let slots = unsafe { slots_mut() };
    for slot in slots.iter_mut() {
        if slot.local_port == 0 {
            slot.local_port = local_port;
            slot.has_pending = false;
            return Ok(());
        }
        if slot.local_port == local_port {
            return Ok(());
        }
    }
    Err(CallbackTransportError::SlotPoolExhausted)
}

fn slot_for_port(local_port: u16) -> Option<&'static mut PortSlot> {
    let slots = unsafe { slots_mut() };
    slots.iter_mut().find(|s| s.local_port == local_port)
}

// ===========================================================================
// C → Rust RX entry point
// ===========================================================================

/// Called by the host C side from the network stack's RX callback
/// for every datagram destined to a port previously bound through
/// this transport.
///
/// # Safety
/// `data` must point to `len` readable bytes for the duration of
/// this call. The caller must run on the same task as the polled
/// executor (no cross-task concurrency with `poll_recv_from`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn simple_someip_callback_transport_on_rx(
    local_port: u16,
    src_addr: u32,
    src_port: u16,
    data: *const u8,
    len: usize,
) {
    if data.is_null() || len == 0 || len > MAX_DATAGRAM {
        return;
    }
    let Some(slot) = slot_for_port(local_port) else {
        return;
    };
    if slot.has_pending {
        // Drop on overflow — single-slot RX assumes the executor
        // drains faster than the wire delivers.
        return;
    }
    // SAFETY: caller guarantees `data..data+len` is readable; `len`
    // is bounded by MAX_DATAGRAM above.
    unsafe {
        core::ptr::copy_nonoverlapping(data, slot.pending.bytes.as_mut_ptr(), len);
    }
    slot.pending.src_addr = src_addr;
    slot.pending.src_port = src_port;
    slot.pending.len = len as u16;
    slot.has_pending = true;
}

// ===========================================================================
// CallbackUdpSocket
// ===========================================================================

/// `AsyncUdpSocket` impl that bridges to a host C transport via
/// `C: TransportCallbacks`.
#[derive(Clone, Copy, Debug)]
pub struct CallbackUdpSocket<C: TransportCallbacks> {
    local_port: u16,
    _marker: PhantomData<fn() -> C>,
}

impl<C: TransportCallbacks> AsyncUdpSocket for CallbackUdpSocket<C> {
    type Error = CallbackTransportError;

    async fn send_to(&self, buf: &[u8], dst: SocketAddrV4) -> Result<(), Self::Error> {
        let rc = C::send_udp(self.local_port, buf, (*dst.ip()).to_bits(), dst.port());
        if rc == 0 {
            Ok(())
        } else {
            Err(CallbackTransportError::SendFailed)
        }
    }

    fn poll_recv_from(
        &self,
        _cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<(usize, SocketAddrV4), Self::Error>> {
        let Some(slot) = slot_for_port(self.local_port) else {
            return Poll::Pending;
        };
        if !slot.has_pending {
            return Poll::Pending;
        }
        let len = slot.pending.len as usize;
        if len > buf.len() {
            slot.has_pending = false;
            return Poll::Ready(Err(CallbackTransportError::OutputBufferTooSmall));
        }
        buf[..len].copy_from_slice(&slot.pending.bytes[..len]);
        let src = SocketAddrV4::new(
            Ipv4Addr::from_bits(slot.pending.src_addr),
            slot.pending.src_port,
        );
        slot.has_pending = false;
        Poll::Ready(Ok((len, src)))
    }

    async fn join_multicast(&self, _group: Ipv4Addr) -> Result<(), Self::Error> {
        // Multicast group join is performed by the C side inside
        // `bind_discovery`. Nothing to do here.
        Ok(())
    }
}

// ===========================================================================
// WrappingMs + CallbackClock
// ===========================================================================

/// Wraparound-safe millisecond instant. The underlying counter
/// (`TransportCallbacks::now_ms`) wraps every ~49 days; comparisons
/// are interpreted modulo `u32::MAX` so durations up to
/// `i32::MAX` ms (~24.8 days) into the future stay correct.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WrappingMs(pub u32);

impl PartialOrd for WrappingMs {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

impl Ord for WrappingMs {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        let diff = self.0.wrapping_sub(other.0) as i32;
        diff.cmp(&0)
    }
}

impl Add<Duration> for WrappingMs {
    type Output = Self;

    fn add(self, rhs: Duration) -> Self {
        let ms = rhs.as_millis().min(u32::MAX as u128) as u32;
        Self(self.0.wrapping_add(ms))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CallbackClock<C: TransportCallbacks> {
    _marker: PhantomData<fn() -> C>,
}

impl<C: TransportCallbacks> Default for CallbackClock<C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C: TransportCallbacks> CallbackClock<C> {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            _marker: PhantomData,
        }
    }
}

impl<C: TransportCallbacks> Clock for CallbackClock<C> {
    type Instant = WrappingMs;

    fn now(&self) -> Self::Instant {
        WrappingMs(C::now_ms())
    }

    fn sleep_until(&self, deadline: Self::Instant) -> impl Future<Output = ()> {
        SleepUntil::<C> {
            deadline,
            _marker: PhantomData,
        }
    }
}

struct SleepUntil<C: TransportCallbacks> {
    deadline: WrappingMs,
    _marker: PhantomData<fn() -> C>,
}

impl<C: TransportCallbacks> Future for SleepUntil<C> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        let now = WrappingMs(C::now_ms());
        if now >= self.deadline {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

// ===========================================================================
// CallbackSocketFactory
// ===========================================================================

#[derive(Clone, Copy, Debug)]
pub struct CallbackSocketFactory<C: TransportCallbacks> {
    _marker: PhantomData<fn() -> C>,
}

impl<C: TransportCallbacks> Default for CallbackSocketFactory<C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C: TransportCallbacks> CallbackSocketFactory<C> {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            _marker: PhantomData,
        }
    }
}

impl<C: TransportCallbacks> SocketFactory for CallbackSocketFactory<C> {
    type Socket = CallbackUdpSocket<C>;
    type Error = CallbackTransportError;

    async fn bind_unicast(
        &self,
        _interface: Ipv4Addr,
        port: u16,
    ) -> Result<(Self::Socket, u16), Self::Error> {
        let bound = C::bind_unicast(port);
        if bound == 0 {
            return Err(CallbackTransportError::BindFailed);
        }
        allocate_slot(bound)?;
        Ok((
            CallbackUdpSocket {
                local_port: bound,
                _marker: PhantomData,
            },
            bound,
        ))
    }

    async fn bind_discovery(
        &self,
        _interface: Ipv4Addr,
        multicast_loopback: bool,
    ) -> Result<Self::Socket, Self::Error> {
        let rc = C::bind_discovery(multicast_loopback);
        if rc != 0 {
            return Err(CallbackTransportError::BindFailed);
        }
        const SD_PORT: u16 = 30490;
        allocate_slot(SD_PORT)?;
        Ok(CallbackUdpSocket {
            local_port: SD_PORT,
            _marker: PhantomData,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping_ms_ordering_before_wrap() {
        assert!(WrappingMs(100) < WrappingMs(200));
        assert!(WrappingMs(200) > WrappingMs(100));
    }

    #[test]
    fn wrapping_ms_ordering_after_wrap() {
        let a = WrappingMs(u32::MAX - 10);
        let b = WrappingMs(5);
        assert!(b > a, "post-wrap instant must compare greater");
    }

    #[test]
    fn wrapping_ms_add_duration_wraps() {
        let a = WrappingMs(u32::MAX - 5);
        let b = a + Duration::from_millis(10);
        assert_eq!(b, WrappingMs(4));
    }
}
