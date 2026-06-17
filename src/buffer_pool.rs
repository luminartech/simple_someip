//! Fixed-capacity pool of `&'static mut [u8]` buffers with claim/release
//! semantics, mirroring the channel pools in this module. A `BufferPool`
//! is declared as a `static` by the consumer; each `claim()` hands out one
//! slot as a `BufferLease` that returns the slot to the pool on drop.
//!
//! Synchronization uses per-slot `AtomicBool` compare-exchange so the same
//! code is valid on the bare-metal target and on std without requiring a
//! `critical-section` implementation.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// Fixed-capacity pool of `LEN`-byte buffers. Declare as a `static` and call
/// [`Self::claim`] to obtain a [`BufferLease`].
///
/// # Const-constructible
///
/// `BufferPool::new()` is `const fn`, so pools can be declared as `static`
/// items initialized at link time with no runtime cost.
///
/// # Synchronization
///
/// Each slot has an independent `AtomicBool` claimed flag. `claim()` scans
/// for the first free slot and atomically claims it via
/// `compare_exchange(false, true, AcqRel, Acquire)`. `Drop` releases via
/// `store(false, Release)`. No global lock is taken; claim and release are
/// individually linearizable.
pub struct BufferPool<const SLOTS: usize, const LEN: usize> {
    // `UnsafeCell` because `claim()` hands out `&'static mut` slices into
    // this store. The `claimed` flags ensure at most one live `&mut` per slot.
    store: UnsafeCell<[[u8; LEN]; SLOTS]>,
    // One atomic flag per slot; `true` = slot is currently claimed.
    claimed: [AtomicBool; SLOTS],
}

// SAFETY: `BufferPool` is Sync because:
// - `claimed` is an array of `AtomicBool`, which is already Sync.
// - Access to `store` is strictly gated: a slot's bytes are only touched
//   while its `claimed` flag is held (compare_exchange'd to true), which
//   ensures at most one live `&mut` per slot at any time.
unsafe impl<const SLOTS: usize, const LEN: usize> Sync for BufferPool<SLOTS, LEN> {}

impl<const SLOTS: usize, const LEN: usize> BufferPool<SLOTS, LEN> {
    /// Create a new, empty pool. All slots are free.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            store: UnsafeCell::new([[0u8; LEN]; SLOTS]),
            claimed: [const { AtomicBool::new(false) }; SLOTS],
        }
    }

    /// Claim a free slot, returning a [`BufferLease`], or `None` if all
    /// `SLOTS` are in use.
    ///
    /// The returned buffer is zeroed before hand-out so a reused slot never
    /// leaks the previous tenant's bytes.
    pub fn claim(&'static self) -> Option<BufferLease> {
        for (idx, flag) in self.claimed.iter().enumerate() {
            // Attempt to atomically claim this slot.
            if flag
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                // SAFETY: we just won the compare_exchange on `claimed[idx]`,
                // so no other `claim()` call holds a reference to slot `idx`.
                // We derive the slot pointer by raw-pointer arithmetic to avoid
                // forming a `&mut` to the whole array (which would alias already-
                // claimed slots). The resulting `&'static mut [u8]` is valid for
                // the lifetime of the `BufferPool` `static`.
                let slot_ptr =
                    unsafe { self.store.get().cast::<[u8; LEN]>().add(idx) };
                // `'static` is sound because `self: &'static BufferPool`, so the
                // backing store outlives the lease; the annotation, not a
                // transmute, carries the lifetime.
                let slot: &'static mut [u8] = unsafe { (*slot_ptr).as_mut_slice() };
                slot.fill(0);
                return Some(BufferLease {
                    buf: slot,
                    claimed_flag: &self.claimed[idx],
                });
            }
        }
        None
    }
}

impl<const SLOTS: usize, const LEN: usize> Default for BufferPool<SLOTS, LEN> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const SLOTS: usize, const LEN: usize> core::fmt::Debug for BufferPool<SLOTS, LEN> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BufferPool")
            .field("slots", &SLOTS)
            .field("len", &LEN)
            .finish_non_exhaustive()
    }
}

/// RAII handle to one claimed buffer from a [`BufferPool`].
///
/// Derefs to `[u8]` for read/write access. Returns the slot to its pool on
/// drop.
pub struct BufferLease {
    buf: &'static mut [u8],
    /// Back-pointer to this slot's claimed flag in the owning pool.
    claimed_flag: &'static AtomicBool,
}

// SAFETY: `BufferLease` owns exclusive access to its slot (enforced by the
// pool's per-slot `AtomicBool`). Both `&'static AtomicBool` and
// `&'static mut [u8]` are Send.
unsafe impl Send for BufferLease {}

impl Deref for BufferLease {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.buf
    }
}

impl DerefMut for BufferLease {
    fn deref_mut(&mut self) -> &mut [u8] {
        self.buf
    }
}

impl Drop for BufferLease {
    fn drop(&mut self) {
        // Release the slot atomically. Any subsequent `claim()` that acquires
        // this flag will see the updated store state.
        self.claimed_flag.store(false, Ordering::Release);
    }
}
