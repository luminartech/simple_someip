//! Fixed-capacity pool of byte buffers with claim/release semantics,
//! mirroring the channel pools in this module. A `BufferPool` is declared as
//! a `static` (bare-metal) or held behind an `Arc` (std/tokio) by the
//! consumer; each claim hands out one slot as a `BufferLease` (a raw
//! `NonNull` slice whose exclusivity is enforced by the per-slot
//! `AtomicBool`, not the borrow checker) that returns the slot on drop.
//!
//! Synchronization uses per-slot `AtomicBool` compare-exchange so the same
//! code is valid on the bare-metal target and on std without requiring a
//! `critical-section` implementation.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, Ordering};

/// Fixed-capacity pool of `LEN`-byte buffers. Declare as a `static` and call
/// [`Self::claim`] to obtain a [`BufferLease`].
///
/// # Const-constructible
///
/// `BufferPool::new()` is `const fn`, so pools can be declared as `static`
/// items initialized at link time with no runtime cost.
///
/// # Minimum slot length
///
/// `LEN` must be at least 16 bytes — the size of a SOME/IP header — or the
/// client silently drops all inbound and rejects all sends. A compile-time
/// `const` assertion in [`Self::new`] enforces this floor. 16 is only the
/// absolute header minimum: in practice a slot must hold the largest expected
/// message (header + payload), realistically one full UDP datagram (see
/// [`crate::UDP_BUFFER_SIZE`]).
///
/// # Synchronization
///
/// Each slot has an independent `AtomicBool` claimed flag. `claim()` scans
/// for the first free slot and atomically claims it via
/// `compare_exchange(false, true, AcqRel, Acquire)`. `Drop` releases via
/// `store(false, Release)`. No global lock is taken; claim and release are
/// individually linearizable.
pub struct BufferPool<const SLOTS: usize, const LEN: usize> {
    // `UnsafeCell` because claims hand out raw `NonNull` slices into this
    // store; the per-slot `claimed` AtomicBool (not the borrow checker) is
    // what guarantees at most one live lease per slot.
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
    ///
    /// # Panics (compile-time)
    ///
    /// A `const` assertion rejects `LEN < 16` at compile time: a slot must be
    /// large enough to hold a 16-byte SOME/IP header, otherwise the client
    /// silently drops all inbound and rejects all sends.
    #[must_use]
    pub const fn new() -> Self {
        // Compile-time floor: a slot must hold at least a SOME/IP header.
        // Placed in `const {}` so it is evaluated during const-eval of every
        // monomorphization that constructs a pool (e.g. the `static` init).
        const {
            assert!(
                LEN >= 16,
                "BufferPool slot must hold at least a 16-byte SOME/IP header"
            );
        };
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
        let (buf, flag) = self.try_claim_slot()?;
        Some(BufferLease {
            buf,
            len: LEN,
            flag,
            // Truly-'static pool (bare-metal static-pool path): nothing to
            // keep alive — the backing store outlives the lease via `'static`.
            // No allocation on this path. The `_owner` field only exists when
            // `alloc` is available; under `bare_metal` it is cfg'd out.
            #[cfg(feature = "_alloc")]
            _owner: None,
        })
    }

    /// Scan for a free slot and atomically claim it. On success returns the
    /// `NonNull` start-of-slot pointer and a `NonNull` to that slot's claimed
    /// flag; on exhaustion returns `None`.
    ///
    /// Both pointers reference memory owned by `self`; the caller is
    /// responsible for keeping `self` alive for as long as the pointers are
    /// used (via `'static` or an `Arc` clone held in the `BufferLease`).
    fn try_claim_slot(&self) -> Option<(NonNull<u8>, NonNull<AtomicBool>)> {
        for (idx, flag) in self.claimed.iter().enumerate() {
            // Attempt to atomically claim this slot.
            if flag
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                // SAFETY: we just won the compare_exchange on `claimed[idx]`,
                // so no other live lease references slot `idx`. We derive the
                // slot pointer by raw-pointer arithmetic to avoid forming a
                // `&mut` to the whole array (which would alias already-claimed
                // slots).
                let slot_ptr = unsafe { self.store.get().cast::<u8>().add(idx * LEN) };
                // Zero the freshly-claimed slot so a reused slot never leaks
                // the previous tenant's bytes.
                //
                // SAFETY: `slot_ptr` is the start of slot `idx`, in bounds for
                // `LEN` bytes, and we hold the exclusive claim on it; no other
                // reference aliases these bytes.
                unsafe { core::ptr::write_bytes(slot_ptr, 0, LEN) };
                // SAFETY: `slot_ptr` derives from `self.store.get()` (non-null)
                // plus an in-bounds offset; `flag` is an element of the
                // `claimed` array (non-null).
                let buf = unsafe { NonNull::new_unchecked(slot_ptr) };
                let flag = NonNull::from(flag);
                return Some((buf, flag));
            }
        }
        None
    }
}

#[cfg(feature = "_alloc")]
impl<const SLOTS: usize, const LEN: usize> BufferPool<SLOTS, LEN> {
    /// Claim a free slot from an `Arc`-backed pool, returning a [`BufferLease`]
    /// that holds an `Arc` clone to keep the pool alive for the lease's
    /// lifetime, or `None` if all `SLOTS` are in use.
    ///
    /// This is the heap-backed counterpart to [`Self::claim`]: the static-pool
    /// path uses `&'static self` and stores `_owner: None`; this path stores
    /// `_owner: Some(arc.clone())` so the pool's backing store (and the slot's
    /// claimed flag) stay valid until the last lease and provider drop. Only
    /// compiled where `alloc` is available (the `_alloc` feature), so the
    /// bare-metal `client,bare_metal` build stays allocation-free.
    pub fn claim_arc(self: &alloc::sync::Arc<Self>) -> Option<BufferLease> {
        let (buf, flag) = self.try_claim_slot()?;
        Some(BufferLease {
            buf,
            len: LEN,
            flag,
            // Keep the Arc'd pool alive for the lease's lifetime. The pool is
            // `Send + Sync` (see the `unsafe impl Sync` above and the `Send`
            // bounds on its contents), so `Arc<Self>` coerces to
            // `Arc<dyn Any + Send + Sync>`.
            _owner: Some(self.clone()),
        })
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
///
/// # Two backing strategies
///
/// - **Static pool** (bare-metal): the pool is a `&'static BufferPool`, so the
///   slot's memory and claimed flag are valid for the program's whole life.
///   `_owner` is `None`; no allocation.
/// - **`Arc`-backed pool** (tokio/std): the pool lives behind an `Arc`. The
///   lease holds an `Arc` clone in `_owner` so the slot's memory and flag stay
///   valid until the last lease *and* the provider drop, at which point the
///   pool is freed — no per-client leak.
pub struct BufferLease {
    /// Start of the claimed slot.
    buf: NonNull<u8>,
    /// Length of the slot, in bytes.
    len: usize,
    /// This slot's claimed flag in the owning pool.
    flag: NonNull<AtomicBool>,
    /// Keeps an `Arc`-backed pool alive for the lease's lifetime; `None` for a
    /// truly-`'static` (bare-metal) pool. Drop order: the flag is cleared
    /// first (the raw pointer is still valid via `_owner` for the Arc case, or
    /// `'static` for the static case), then `_owner` drops, releasing the
    /// pool's last reference if this was the final holder.
    #[cfg(feature = "_alloc")]
    _owner: Option<alloc::sync::Arc<dyn core::any::Any + Send + Sync>>,
}

// SAFETY: `BufferLease` is `Send` because:
//  - The lease owns exclusive access to its slot, enforced by the pool's
//    per-slot `AtomicBool` (won via compare_exchange in `try_claim_slot`); no
//    other live lease can reference the same slot bytes.
//  - The raw `NonNull<u8>` / `NonNull<AtomicBool>` pointers are themselves
//    `Send` only by this `unsafe impl`; they reference memory kept alive
//    either by `'static` (static path) or by the `Arc` in `_owner`, which is
//    `Arc<dyn Any + Send + Sync>` and hence `Send`. Sending the lease to
//    another thread moves all of these together, so the slot's memory and
//    flag remain valid and exclusively owned.
unsafe impl Send for BufferLease {}

impl Deref for BufferLease {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: `buf` points at the start of an exclusively-claimed slot of
        // `len` bytes, kept alive by `_owner`/`'static`. We hold `&self`, so
        // an immutable slice is sound (no concurrent `&mut` exists — the
        // claimed flag guarantees a single live lease per slot).
        unsafe { core::slice::from_raw_parts(self.buf.as_ptr(), self.len) }
    }
}

impl DerefMut for BufferLease {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as in `deref`, plus we hold `&mut self`, so a mutable slice
        // is the unique reference to these bytes.
        unsafe { core::slice::from_raw_parts_mut(self.buf.as_ptr(), self.len) }
    }
}

impl Drop for BufferLease {
    fn drop(&mut self) {
        // Release the slot atomically. The flag memory is still valid here:
        // for the static path it is `'static`; for the Arc path `_owner` (which
        // drops *after* this block) still holds a live reference to the pool.
        // Any subsequent claim that acquires this flag will see the updated
        // store state.
        //
        // SAFETY: `flag` references this slot's `AtomicBool` inside the pool,
        // valid for the reasons above.
        unsafe { self.flag.as_ref() }.store(false, Ordering::Release);
        // `_owner` (if any) drops after this, releasing the pool's last
        // reference when this is the final holder.
    }
}
