//! Shared-pool RX mailbox for the bare-metal callback transport.
//!
//! A platform's RX interrupt/callback pushes inbound datagrams into the
//! first free slot (any port → any slot); [`CallbackSocket`] consumers
//! poll for a slot matching their port and take it. Single-producer
//! (the platform RX callback) / single-consumer-per-port.
//!
//! The instance is owned by the consuming project (so it controls link
//! placement — e.g. a specific RAM section); the runtime borrows it by
//! `&'static`. `SLOTS` × `CAP` bytes of storage.
//!
//! [`CallbackSocket`]: super::transport::CallbackSocket

use core::cell::UnsafeCell;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicUsize, Ordering};

/// One mailbox slot holding a single pending datagram of up to `CAP`
/// bytes plus its source/port metadata.
pub struct RxSlot<const CAP: usize> {
    has_datagram: AtomicBool,
    local_port: AtomicU16,
    src_addr: AtomicU32,
    src_port: AtomicU16,
    len: AtomicUsize,
    data: UnsafeCell<[u8; CAP]>,
}

// SAFETY: access is coordinated by `has_datagram` (Release on fill,
// Acquire on read) between a single producer and a single consumer; the
// `UnsafeCell` is only written while `has_datagram == false`.
unsafe impl<const CAP: usize> Sync for RxSlot<CAP> {}

impl<const CAP: usize> RxSlot<CAP> {
    const fn new() -> Self {
        Self {
            has_datagram: AtomicBool::new(false),
            local_port: AtomicU16::new(0),
            src_addr: AtomicU32::new(0),
            src_port: AtomicU16::new(0),
            len: AtomicUsize::new(0),
            data: UnsafeCell::new([0u8; CAP]),
        }
    }
}

/// Fixed-capacity shared RX pool: `SLOTS` slots of `CAP` bytes each.
pub struct RxMailbox<const SLOTS: usize, const CAP: usize> {
    slots: [RxSlot<CAP>; SLOTS],
}

impl<const SLOTS: usize, const CAP: usize> Default for RxMailbox<SLOTS, CAP> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const SLOTS: usize, const CAP: usize> RxMailbox<SLOTS, CAP> {
    /// Create an empty mailbox. `const` so it can initialize a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: [const { RxSlot::new() }; SLOTS],
        }
    }

    /// Per-slot datagram capacity in bytes.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        CAP
    }

    /// Push one inbound datagram into the first free slot. Returns `false`
    /// if the pool was full (datagram dropped). Oversized payloads are
    /// truncated to `CAP`.
    ///
    /// # Safety
    /// `buf` must be valid for `len` bytes.
    pub unsafe fn push(
        &self,
        local_port: u16,
        src_addr: u32,
        src_port: u16,
        buf: *const u8,
        len: usize,
    ) -> bool {
        for slot in &self.slots {
            if slot.has_datagram.load(Ordering::Acquire) {
                continue;
            }
            let dst = unsafe { &mut *slot.data.get() };
            let n = if len < CAP { len } else { CAP };
            // SAFETY: caller guarantees `buf` valid for `len >= n` bytes;
            // `dst` is `CAP >= n` bytes; this slot is free (no concurrent
            // reader until we Release `has_datagram`).
            unsafe { core::ptr::copy_nonoverlapping(buf, dst.as_mut_ptr(), n) };
            slot.len.store(n, Ordering::Release);
            slot.src_addr.store(src_addr, Ordering::Release);
            slot.src_port.store(src_port, Ordering::Release);
            slot.local_port.store(local_port, Ordering::Release);
            slot.has_datagram.store(true, Ordering::Release);
            return true;
        }
        false
    }

    /// Take the next pending datagram for `port` into `out`, freeing the
    /// slot. Returns `(bytes_copied, source, truncated)` or `None` if no
    /// datagram is pending for that port. `truncated` is set when the
    /// stored datagram was larger than `out`.
    pub fn take(&self, port: u16, out: &mut [u8]) -> Option<(usize, SocketAddrV4, bool)> {
        for slot in &self.slots {
            if !slot.has_datagram.load(Ordering::Acquire) {
                continue;
            }
            if slot.local_port.load(Ordering::Acquire) != port {
                continue;
            }
            let src_addr = slot.src_addr.load(Ordering::Acquire);
            let src_port = slot.src_port.load(Ordering::Acquire);
            let datagram_len = slot.len.load(Ordering::Acquire);
            let copy_len = datagram_len.min(out.len());
            // SAFETY: slot is filled (Acquire above); the producer does not
            // touch a filled slot until we clear `has_datagram` below.
            unsafe {
                let src_ptr = (*slot.data.get()).as_ptr();
                core::ptr::copy_nonoverlapping(src_ptr, out.as_mut_ptr(), copy_len);
            }
            slot.has_datagram.store(false, Ordering::Release);
            let src = SocketAddrV4::new(Ipv4Addr::from(src_addr.to_be_bytes()), src_port);
            return Some((copy_len, src, datagram_len > copy_len));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_then_take_round_trips_port_and_payload() {
        let mb: RxMailbox<2, 16> = RxMailbox::new();
        let payload = [1u8, 2, 3, 4];
        // 192.0.2.7 in host byte order.
        let src_addr = u32::from_be_bytes([192, 0, 2, 7]);
        assert!(unsafe { mb.push(30490, src_addr, 40000, payload.as_ptr(), payload.len()) });

        // Wrong port -> nothing.
        let mut buf = [0u8; 16];
        assert!(mb.take(10000, &mut buf).is_none());

        let (n, src, trunc) = mb.take(30490, &mut buf).expect("datagram for port");
        assert_eq!(&buf[..n], &payload);
        assert_eq!(src, SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 7), 40000));
        assert!(!trunc);
        // Slot freed.
        assert!(mb.take(30490, &mut buf).is_none());
    }

    #[test]
    fn full_pool_drops() {
        let mb: RxMailbox<1, 8> = RxMailbox::new();
        let d = [9u8; 4];
        assert!(unsafe { mb.push(1, 0, 0, d.as_ptr(), d.len()) });
        // Pool full (1 slot) -> second push dropped.
        assert!(!unsafe { mb.push(1, 0, 0, d.as_ptr(), d.len()) });
    }

    #[test]
    fn take_into_short_buffer_sets_truncated() {
        let mb: RxMailbox<1, 8> = RxMailbox::new();
        let d = [1u8, 2, 3];
        assert!(unsafe { mb.push(1, 0, 0, d.as_ptr(), d.len()) });
        let mut buf = [0u8; 2];
        let (n, _src, trunc) = mb.take(1, &mut buf).unwrap();
        assert_eq!(n, 2);
        assert!(trunc, "3-byte datagram into 2-byte buffer must flag truncation");
    }
}
