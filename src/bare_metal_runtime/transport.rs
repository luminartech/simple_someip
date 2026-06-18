//! Callback-driven `TransportSocket` / `TransportFactory` / `Timer` for
//! bare-metal targets.
//!
//! Instead of a project supplying concrete socket/timer *types* (generics,
//! which `#[embassy_executor::task]` can't accept) it supplies plain C-ABI
//! **function pointers** at runtime: send a UDP datagram, and read the
//! monotonic millisecond clock. Inbound datagrams are delivered out-of-band
//! into an [`RxMailbox`] the project owns. This makes the whole runtime
//! concrete, so it can live in this library and be reused by any platform.
//!
//! The poll-futures mirror a typical lwIP integration: send is synchronous
//! (resolves on first poll); recv polls the mailbox (re-wakes + `Pending`
//! when empty, matching a tick-polled executor); sleep polls the clock.

use core::future::Future;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::pin::Pin;
use core::task::{Context, Poll};
use core::time::Duration;

use crate::transport::{
    IoErrorKind, ReceivedDatagram, SocketOptions, Timer, TransportError, TransportFactory,
    TransportSocket,
};

use super::mailbox::RxMailbox;

/// Transmit one UDP datagram. Matches a typical lwIP send shim:
/// `(local_port, buf, len, dst_addr_be_as_host_u32, dst_port) -> 0 on ok`.
/// `dst_addr` is the IPv4 address as a host-order `u32` (big-endian octets
/// reassembled), the same convention the SD/notification code uses.
pub type SendFn =
    extern "C" fn(local_port: u16, buf: *const u8, len: usize, dst_addr: u32, dst_port: u16) -> i32;

/// Read the monotonic clock in milliseconds (wraps at `u32::MAX`).
pub type NowMsFn = extern "C" fn() -> u32;

/// Borrowed handle to the platform callbacks + RX mailbox, shared by every
/// socket the factory hands out. `'m` is the mailbox/lifetime.
#[derive(Clone, Copy)]
pub struct Platform<'m, const SLOTS: usize, const CAP: usize> {
    pub send: SendFn,
    pub now_ms: NowMsFn,
    pub mailbox: &'m RxMailbox<SLOTS, CAP>,
    /// Local interface address (host-order `u32`) for `local_addr`.
    pub interface: u32,
}

/// Port-typed UDP socket marker. The PCB lives in the platform; `send_to`
/// calls [`Platform::send`], `recv_from` polls [`Platform::mailbox`].
pub struct CallbackSocket<'m, const SLOTS: usize, const CAP: usize> {
    port: u16,
    plat: Platform<'m, SLOTS, CAP>,
}

pub struct CbSendFuture<'a, const SLOTS: usize, const CAP: usize> {
    port: u16,
    send: SendFn,
    buf: &'a [u8],
    target: SocketAddrV4,
}

impl<const SLOTS: usize, const CAP: usize> Future for CbSendFuture<'_, SLOTS, CAP> {
    type Output = Result<(), TransportError>;
    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let dst = u32::from_be_bytes(self.target.ip().octets());
        let rc = (self.send)(
            self.port,
            self.buf.as_ptr(),
            self.buf.len(),
            dst,
            self.target.port(),
        );
        if rc == 0 {
            Poll::Ready(Ok(()))
        } else {
            Poll::Ready(Err(TransportError::Io(IoErrorKind::Other)))
        }
    }
}

pub struct CbRecvFuture<'a, 'm, const SLOTS: usize, const CAP: usize> {
    port: u16,
    mailbox: &'m RxMailbox<SLOTS, CAP>,
    buf: &'a mut [u8],
}

impl<const SLOTS: usize, const CAP: usize> Future for CbRecvFuture<'_, '_, SLOTS, CAP> {
    type Output = Result<ReceivedDatagram, TransportError>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Reborrow split so we can pass `buf` mutably while reading `port`.
        let this = &mut *self;
        if let Some((n, src, truncated)) = this.mailbox.take(this.port, this.buf) {
            Poll::Ready(Ok(ReceivedDatagram {
                bytes_received: n,
                source: src,
                truncated,
            }))
        } else {
            // Tick-polled executor: re-wake so the next executor poll
            // re-checks the mailbox (filled out-of-band by the RX callback).
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

impl<'m, const SLOTS: usize, const CAP: usize> TransportSocket for CallbackSocket<'m, SLOTS, CAP> {
    type SendFuture<'a>
        = CbSendFuture<'a, SLOTS, CAP>
    where
        Self: 'a;
    type RecvFuture<'a>
        = CbRecvFuture<'a, 'm, SLOTS, CAP>
    where
        Self: 'a;

    fn send_to<'a>(&'a self, buf: &'a [u8], target: SocketAddrV4) -> Self::SendFuture<'a> {
        CbSendFuture {
            port: self.port,
            send: self.plat.send,
            buf,
            target,
        }
    }

    fn recv_from<'a>(&'a self, buf: &'a mut [u8]) -> Self::RecvFuture<'a> {
        CbRecvFuture {
            port: self.port,
            mailbox: self.plat.mailbox,
            buf,
        }
    }

    fn local_addr(&self) -> Result<SocketAddrV4, TransportError> {
        Ok(SocketAddrV4::new(
            Ipv4Addr::from(self.plat.interface.to_be_bytes()),
            self.port,
        ))
    }

    fn join_multicast_v4(&self, _group: Ipv4Addr, _iface: Ipv4Addr) -> Result<(), TransportError> {
        // The platform joined the group at PCB bind time.
        Ok(())
    }

    fn leave_multicast_v4(&self, _group: Ipv4Addr, _iface: Ipv4Addr) -> Result<(), TransportError> {
        Ok(())
    }

    fn max_datagram_size(&self) -> usize {
        CAP
    }
}

/// Hands out [`CallbackSocket`] markers. PCBs are pre-bound by the platform
/// (the catalog ports are bound during init), so `bind` just returns a
/// marker for the requested port.
#[derive(Clone, Copy)]
pub struct CallbackFactory<'m, const SLOTS: usize, const CAP: usize> {
    plat: Platform<'m, SLOTS, CAP>,
}

impl<'m, const SLOTS: usize, const CAP: usize> CallbackFactory<'m, SLOTS, CAP> {
    #[must_use]
    pub const fn new(plat: Platform<'m, SLOTS, CAP>) -> Self {
        Self { plat }
    }

    /// Construct a socket marker for `port` directly (the PCB is already
    /// bound by the platform). Avoids the async `bind` when the caller
    /// holds the port.
    #[must_use]
    pub const fn socket(&self, port: u16) -> CallbackSocket<'m, SLOTS, CAP> {
        CallbackSocket {
            port,
            plat: self.plat,
        }
    }
}

pub struct CbBindFuture<'m, const SLOTS: usize, const CAP: usize> {
    port: u16,
    plat: Platform<'m, SLOTS, CAP>,
}

impl<'m, const SLOTS: usize, const CAP: usize> Future for CbBindFuture<'m, SLOTS, CAP> {
    type Output = Result<CallbackSocket<'m, SLOTS, CAP>, TransportError>;
    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(Ok(CallbackSocket {
            port: self.port,
            plat: self.plat,
        }))
    }
}

impl<'m, const SLOTS: usize, const CAP: usize> TransportFactory
    for CallbackFactory<'m, SLOTS, CAP>
{
    type Socket = CallbackSocket<'m, SLOTS, CAP>;
    type BindFuture<'a>
        = CbBindFuture<'m, SLOTS, CAP>
    where
        Self: 'a;

    fn bind<'a>(&'a self, addr: SocketAddrV4, _options: &'a SocketOptions) -> Self::BindFuture<'a> {
        CbBindFuture {
            port: addr.port(),
            plat: self.plat,
        }
    }
}

/// `Timer` backed by a monotonic-ms callback.
#[derive(Clone, Copy)]
pub struct CallbackTimer {
    now_ms: NowMsFn,
}

impl CallbackTimer {
    #[must_use]
    pub const fn new(now_ms: NowMsFn) -> Self {
        Self { now_ms }
    }
}

pub struct CbSleepFuture {
    now_ms: NowMsFn,
    deadline_ms: u32,
}

impl Future for CbSleepFuture {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let now = (self.now_ms)();
        // i32 diff stays correct across 32-bit wraparound.
        if self.deadline_ms.wrapping_sub(now) as i32 <= 0 {
            Poll::Ready(())
        } else {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

impl Timer for CallbackTimer {
    type SleepFuture<'a>
        = CbSleepFuture
    where
        Self: 'a;
    fn sleep(&self, duration: Duration) -> Self::SleepFuture<'_> {
        let now = (self.now_ms)();
        #[allow(clippy::cast_possible_truncation)]
        let dur_ms = duration.as_millis().min(u128::from(u32::MAX)) as u32;
        CbSleepFuture {
            now_ms: self.now_ms,
            deadline_ms: now.wrapping_add(dur_ms),
        }
    }
}
