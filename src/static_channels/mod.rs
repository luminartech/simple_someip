//! Static-pool no-alloc backend for [`ChannelFactory`].
//!
//! `crate::embassy_channels::EmbassySyncChannels` (under
//! `feature = "embassy_channels"`) heap-allocates one
//! `Arc<Channel<...>>` per `oneshot()` / `bounded()` / `unbounded()`
//! call. On a real bare-metal target that violates the strategic
//! "zero heap after `Client::new` returns" goal, because
//! `Client`'s run-loop awaits a oneshot for every request-response
//! pair.
//!
//! This module hands out `&'static` references into pre-allocated
//! `static` pools instead. The user declares pools (typically via
//! the [`define_static_channels!`](crate::define_static_channels) macro)
//! sized to their workload's high-water mark; once seeded, no further
//! allocation occurs.
//!
//! # Per-`T` `*Pooled<MyChannels>` impls
//!
//! [`ChannelFactory`] requires each constructor method to have
//! `T: *Pooled<Self>`. Static-pool consumers publish per-`T`
//! impls that route to the appropriate pool. The
//! [`define_static_channels!`](crate::define_static_channels) macro
//! generates them; the primitives in this module are the runtime they
//! call into.
//!
//! # Pool exhaustion
//!
//! If an `OneshotPool::claim()` / `MpscPool::claim_bounded()` call finds the
//! pool empty it returns `None`. The trait method
//! `*Pooled::*_pair() -> (Sender, Receiver)` cannot return `None` —
//! it has no error channel — so generated impls **panic** on
//! exhaustion. Sizing the pool to the workload's high-water mark is
//! the user's responsibility; an exhaustion panic is a config error,
//! not a runtime error.
//!
//! # Cancellation semantics
//!
//! - **Sender drop without `send`**: the slot's cancellation flag is
//!   set; the receiver's pending `recv()` resolves to
//!   `Err(OneshotCancelled)` (oneshot) or `None` (bounded /
//!   unbounded mpsc, after the last sender drops).
//! - **Receiver drop**: any pending value in the slot is dropped when
//!   the slot is reclaimed. Bounded senders blocked on a full channel
//!   are all woken via the slot's `MultiWakerRegistration` so each
//!   resolves to `Err(())` on its next poll — including cloned senders
//!   beyond the registration's static cap, which fall back to the
//!   "wake-on-next-register" path.

#![allow(clippy::module_name_repetitions)]

use core::cell::{Cell, RefCell};
use core::future::{Future, poll_fn};
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use core::task::Poll;

use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::waitqueue::{AtomicWaker, MultiWakerRegistration};

/// Maximum number of distinct waiting senders we wake on receiver drop.
/// More than this and the multi-waker auto-wakes-and-clears on the next
/// register, so the close path remains correct under any sender count —
/// it just degrades to "wake on next register" for the overflow case.
const SEND_WAKER_CAP: usize = 8;

use crate::transport::{
    MpscRecv, MpscSend, OneshotCancelled, OneshotRecv, OneshotSend, UnboundedRecv, UnboundedSend,
};

// ── Oneshot ───────────────────────────────────────────────────────────

const O_SENDER_ALIVE: u8 = 0b001;
const O_RECEIVER_ALIVE: u8 = 0b010;
const O_CANCELLED: u8 = 0b100;

/// One slot of a [`OneshotPool`]. Const-constructible so a `static`
/// array of slots can be initialized in const context.
pub struct OneshotSlot<T: Send + 'static> {
    chan: Channel<CriticalSectionRawMutex, T, 1>,
    /// Woken by the sender's drop when it cancels without sending.
    /// (The chan's internal waker handles the value-arrival path.)
    cancel_waker: AtomicWaker,
    /// `O_SENDER_ALIVE | O_RECEIVER_ALIVE | O_CANCELLED` bitmask.
    state: AtomicU8,
    /// Free-list link (1-based pool index; 0 = none).
    next_free: AtomicUsize,
}

impl<T: Send + 'static> OneshotSlot<T> {
    /// Const-constructible empty slot.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            chan: Channel::new(),
            cancel_waker: AtomicWaker::new(),
            state: AtomicU8::new(0),
            next_free: AtomicUsize::new(0),
        }
    }
}

impl<T: Send + 'static> Default for OneshotSlot<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Reclaim hook used by [`StaticOneshotSender`] / [`StaticOneshotReceiver`]
/// in their `Drop` impls. Erases the pool's `POOL_SIZE` so handles do
/// not carry it.
trait OneshotReclaim<T: Send + 'static>: Send + Sync + 'static {
    fn release(&self, slot: &'static OneshotSlot<T>);
}

/// A pool of [`OneshotSlot`]s. Place in a `static` and call
/// [`Self::claim`] to obtain a sender/receiver pair.
pub struct OneshotPool<T: Send + 'static, const POOL_SIZE: usize> {
    slots: [OneshotSlot<T>; POOL_SIZE],
    free_head: BlockingMutex<CriticalSectionRawMutex, Cell<usize>>,
    seeded: AtomicBool,
}

impl<T: Send + 'static, const POOL_SIZE: usize> OneshotPool<T, POOL_SIZE> {
    /// Const-constructible empty pool. Free-list is seeded lazily on
    /// the first [`Self::claim`].
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: [const { OneshotSlot::new() }; POOL_SIZE],
            free_head: BlockingMutex::new(Cell::new(0)),
            seeded: AtomicBool::new(false),
        }
    }

    /// Try to obtain a fresh sender/receiver pair. Returns `None` if
    /// the pool is exhausted.
    pub fn claim(&'static self) -> Option<(StaticOneshotSender<T>, StaticOneshotReceiver<T>)> {
        self.ensure_seeded();
        let slot = self.pop_free()?;
        slot.state
            .store(O_SENDER_ALIVE | O_RECEIVER_ALIVE, Ordering::Release);
        // No stale value should be in the channel (we drained on
        // release), but be defensive.
        let _ = slot.chan.try_receive();
        Some((
            StaticOneshotSender {
                slot,
                pool: self,
                sent: false,
            },
            StaticOneshotReceiver { slot, pool: self },
        ))
    }

    fn ensure_seeded(&self) {
        // Seed the free list under the same mutex `pop_free` takes, so a
        // racing claimer cannot win the mutex between our (won) CAS and
        // our `free_head.lock(|h| h.set(1))` and observe `head == 0`.
        // The `seeded` atomic is only an optimisation — once true, we
        // skip the mutex acquire entirely.
        if self.seeded.load(Ordering::Acquire) {
            return;
        }
        self.free_head.lock(|h| {
            // Re-check under the mutex; another claimer may have seeded
            // while we were contending for it.
            if self.seeded.load(Ordering::Acquire) {
                return;
            }
            // Link slots[0] -> slots[1] -> ... -> slots[N-1] -> 0.
            for i in 0..POOL_SIZE {
                let next = if i + 1 < POOL_SIZE { i + 2 } else { 0 };
                self.slots[i].next_free.store(next, Ordering::Release);
            }
            h.set(1);
            self.seeded.store(true, Ordering::Release);
        });
    }

    fn pop_free(&self) -> Option<&OneshotSlot<T>> {
        self.free_head.lock(|h| {
            let head = h.get();
            if head == 0 {
                return None;
            }
            let slot = &self.slots[head - 1];
            let next = slot.next_free.load(Ordering::Acquire);
            h.set(next);
            slot.next_free.store(0, Ordering::Release);
            Some(slot)
        })
    }
}

impl<T: Send + 'static, const POOL_SIZE: usize> Default for OneshotPool<T, POOL_SIZE> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Send + 'static, const POOL_SIZE: usize> OneshotReclaim<T> for OneshotPool<T, POOL_SIZE> {
    fn release(&self, slot: &'static OneshotSlot<T>) {
        let base = self.slots.as_ptr() as usize;
        let here = core::ptr::from_ref::<OneshotSlot<T>>(slot) as usize;
        let stride = core::mem::size_of::<OneshotSlot<T>>();
        debug_assert!(stride > 0, "OneshotSlot must be sized");
        debug_assert!(here >= base);
        let idx = (here - base) / stride;
        debug_assert!(idx < POOL_SIZE, "slot does not belong to this pool");
        // Drop any stale value still in the channel.
        let _ = slot.chan.try_receive();
        // Overwrite any stale waker still registered by the previous
        // tenant so the next claim's first registration does not wake
        // (and potentially poke) a defunct task. `register` overwrites
        // the previous slot if the new waker would-wake a different
        // task, so registering the noop waker effectively clears it.
        slot.cancel_waker.register(core::task::Waker::noop());
        slot.state.store(0, Ordering::Release);
        self.free_head.lock(|h| {
            slot.next_free.store(h.get(), Ordering::Release);
            h.set(idx + 1);
        });
    }
}

/// Send half of a static-pool oneshot.
pub struct StaticOneshotSender<T: Send + 'static> {
    slot: &'static OneshotSlot<T>,
    pool: &'static dyn OneshotReclaim<T>,
    sent: bool,
}

impl<T: Send + 'static> OneshotSend<T> for StaticOneshotSender<T> {
    fn send(mut self, value: T) -> Result<(), T> {
        // Refuse to send if the receiver has already dropped.
        // (A subsequent receiver drop between this check and try_send
        // is harmless — the value lands in the slot and is drained on
        // slot release.)
        if self.slot.state.load(Ordering::Acquire) & O_RECEIVER_ALIVE == 0 {
            return Err(value);
        }
        match self.slot.chan.try_send(value) {
            Ok(()) => {
                self.sent = true;
                Ok(())
            }
            Err(embassy_sync::channel::TrySendError::Full(v)) => Err(v),
        }
    }
}

impl<T: Send + 'static> Drop for StaticOneshotSender<T> {
    fn drop(&mut self) {
        if !self.sent {
            self.slot.state.fetch_or(O_CANCELLED, Ordering::AcqRel);
            self.slot.cancel_waker.wake();
        }
        let prev = self.slot.state.fetch_and(!O_SENDER_ALIVE, Ordering::AcqRel);
        let after = prev & !O_SENDER_ALIVE;
        if (after & O_RECEIVER_ALIVE) == 0 {
            self.pool.release(self.slot);
        }
    }
}

/// Receive half of a static-pool oneshot.
pub struct StaticOneshotReceiver<T: Send + 'static> {
    slot: &'static OneshotSlot<T>,
    pool: &'static dyn OneshotReclaim<T>,
}

impl<T: Send + 'static> OneshotRecv<T> for StaticOneshotReceiver<T> {
    async fn recv(self) -> Result<T, OneshotCancelled> {
        let slot = self.slot;
        let result = poll_fn(move |cx| {
            // 1. Try the channel first.
            if let Ok(v) = slot.chan.try_receive() {
                return Poll::Ready(Ok(v));
            }
            // 2. Check cancellation.
            if slot.state.load(Ordering::Acquire) & O_CANCELLED != 0 {
                return Poll::Ready(Err(OneshotCancelled));
            }
            // 3. Register on the cancel waker.
            slot.cancel_waker.register(cx.waker());
            // 4. Register on the channel's internal waker by polling
            //    a transient receive future. embassy-sync registers
            //    the waker on poll and does not unregister on drop.
            {
                let mut fut = slot.chan.receive();
                // SAFETY: `fut` is stack-pinned, polled exactly
                // once, then dropped before this scope ends. No
                // reference to `fut` escapes.
                let pinned = unsafe { Pin::new_unchecked(&mut fut) };
                if let Poll::Ready(v) = pinned.poll(cx) {
                    return Poll::Ready(Ok(v));
                }
            }
            // 5. Final re-check to close the lost-wakeup window
            //    between the early try_receive and the waker
            //    registrations.
            if let Ok(v) = slot.chan.try_receive() {
                return Poll::Ready(Ok(v));
            }
            if slot.state.load(Ordering::Acquire) & O_CANCELLED != 0 {
                return Poll::Ready(Err(OneshotCancelled));
            }
            Poll::Pending
        })
        .await;
        // `self` drops here on return, running receiver-side bookkeeping.
        drop(self);
        result
    }
}

impl<T: Send + 'static> Drop for StaticOneshotReceiver<T> {
    fn drop(&mut self) {
        let prev = self
            .slot
            .state
            .fetch_and(!O_RECEIVER_ALIVE, Ordering::AcqRel);
        let after = prev & !O_RECEIVER_ALIVE;
        if (after & O_SENDER_ALIVE) == 0 {
            self.pool.release(self.slot);
        }
    }
}

// ── Mpsc (bounded + unbounded share the slot/pool machinery) ──────────

/// One slot of an [`MpscPool`]. Const-constructible.
///
/// Used by both bounded ([`StaticBoundedSender`] /
/// [`StaticBoundedReceiver`]) and unbounded ([`StaticUnboundedSender`]
/// / [`StaticUnboundedReceiver`]) pools — the public sender/receiver
/// types differ, but the slot machinery is shared.
pub struct MpscSlot<T: Send + 'static, const SLOT_CAP: usize> {
    chan: Channel<CriticalSectionRawMutex, T, SLOT_CAP>,
    /// Wakes the receiver on close.
    close_waker: AtomicWaker,
    /// Wakes senders that are `await`ing on a full channel when the
    /// receiver drops. Multi-slot so all cloned senders blocked on a
    /// full channel are unblocked on close — a single `AtomicWaker`
    /// would deadlock the non-most-recent senders permanently.
    send_wakers:
        BlockingMutex<CriticalSectionRawMutex, RefCell<MultiWakerRegistration<SEND_WAKER_CAP>>>,
    /// Number of live senders (clones) + 1 if receiver is alive.
    /// 0 → slot returns to free list.
    refcount: AtomicUsize,
    /// Set when the last sender drops while receiver is still alive,
    /// so the receiver's `recv()` resolves to `None`. Also set when the
    /// receiver drops, so subsequent sender ops return `Err`.
    closed: AtomicBool,
    next_free: AtomicUsize,
}

impl<T: Send + 'static, const SLOT_CAP: usize> MpscSlot<T, SLOT_CAP> {
    /// Const-constructible empty slot.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            chan: Channel::new(),
            close_waker: AtomicWaker::new(),
            send_wakers: BlockingMutex::new(RefCell::new(MultiWakerRegistration::new())),
            refcount: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            next_free: AtomicUsize::new(0),
        }
    }
}

impl<T: Send + 'static, const SLOT_CAP: usize> Default for MpscSlot<T, SLOT_CAP> {
    fn default() -> Self {
        Self::new()
    }
}

trait MpscReclaim<T: Send + 'static, const SLOT_CAP: usize>: Send + Sync + 'static {
    fn release(&self, slot: &'static MpscSlot<T, SLOT_CAP>);
}

/// A pool of [`MpscSlot`]s. Place in a `static` and call
/// [`Self::claim_bounded`] or [`Self::claim_unbounded`].
pub struct MpscPool<T: Send + 'static, const POOL_SIZE: usize, const SLOT_CAP: usize> {
    slots: [MpscSlot<T, SLOT_CAP>; POOL_SIZE],
    free_head: BlockingMutex<CriticalSectionRawMutex, Cell<usize>>,
    seeded: AtomicBool,
}

impl<T: Send + 'static, const POOL_SIZE: usize, const SLOT_CAP: usize>
    MpscPool<T, POOL_SIZE, SLOT_CAP>
{
    /// Const-constructible empty pool.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: [const { MpscSlot::new() }; POOL_SIZE],
            free_head: BlockingMutex::new(Cell::new(0)),
            seeded: AtomicBool::new(false),
        }
    }

    /// Claim a slot for use as a bounded MPSC channel.
    pub fn claim_bounded(
        &'static self,
    ) -> Option<(
        StaticBoundedSender<T, SLOT_CAP>,
        StaticBoundedReceiver<T, SLOT_CAP>,
    )> {
        let slot = self.claim_inner()?;
        Some((
            StaticBoundedSender { slot, pool: self },
            StaticBoundedReceiver { slot, pool: self },
        ))
    }

    /// Claim a slot for use as an unbounded MPSC channel. (Embassy-sync
    /// has no truly unbounded channel; this uses `SLOT_CAP` as the
    /// effective capacity.)
    pub fn claim_unbounded(
        &'static self,
    ) -> Option<(
        StaticUnboundedSender<T, SLOT_CAP>,
        StaticUnboundedReceiver<T, SLOT_CAP>,
    )> {
        let slot = self.claim_inner()?;
        Some((
            StaticUnboundedSender { slot, pool: self },
            StaticUnboundedReceiver { slot, pool: self },
        ))
    }

    fn claim_inner(&'static self) -> Option<&'static MpscSlot<T, SLOT_CAP>> {
        self.ensure_seeded();
        let slot = self.pop_free()?;
        slot.refcount.store(2, Ordering::Release); // 1 sender + 1 receiver.
        slot.closed.store(false, Ordering::Release);
        // Defensive: drain any stale value.
        while slot.chan.try_receive().is_ok() {}
        Some(slot)
    }

    fn ensure_seeded(&self) {
        // See `OneshotPool::ensure_seeded` for the rationale: seeding
        // must happen under the same mutex `pop_free` takes, otherwise a
        // racing claimer can win the mutex first and observe an empty
        // free list.
        if self.seeded.load(Ordering::Acquire) {
            return;
        }
        self.free_head.lock(|h| {
            if self.seeded.load(Ordering::Acquire) {
                return;
            }
            for i in 0..POOL_SIZE {
                let next = if i + 1 < POOL_SIZE { i + 2 } else { 0 };
                self.slots[i].next_free.store(next, Ordering::Release);
            }
            h.set(1);
            self.seeded.store(true, Ordering::Release);
        });
    }

    fn pop_free(&self) -> Option<&MpscSlot<T, SLOT_CAP>> {
        self.free_head.lock(|h| {
            let head = h.get();
            if head == 0 {
                return None;
            }
            let slot = &self.slots[head - 1];
            let next = slot.next_free.load(Ordering::Acquire);
            h.set(next);
            slot.next_free.store(0, Ordering::Release);
            Some(slot)
        })
    }
}

impl<T: Send + 'static, const POOL_SIZE: usize, const SLOT_CAP: usize> Default
    for MpscPool<T, POOL_SIZE, SLOT_CAP>
{
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Send + 'static, const POOL_SIZE: usize, const SLOT_CAP: usize> MpscReclaim<T, SLOT_CAP>
    for MpscPool<T, POOL_SIZE, SLOT_CAP>
{
    fn release(&self, slot: &'static MpscSlot<T, SLOT_CAP>) {
        let base = self.slots.as_ptr() as usize;
        let here = core::ptr::from_ref::<MpscSlot<T, SLOT_CAP>>(slot) as usize;
        let stride = core::mem::size_of::<MpscSlot<T, SLOT_CAP>>();
        debug_assert!(stride > 0);
        debug_assert!(here >= base);
        let idx = (here - base) / stride;
        debug_assert!(idx < POOL_SIZE);
        while slot.chan.try_receive().is_ok() {}
        // Overwrite any stale wakers still registered by the previous
        // tenant so the next claim's first registration does not poke
        // a defunct task.
        slot.close_waker.register(core::task::Waker::noop());
        slot.send_wakers.lock(|w| w.borrow_mut().wake());
        slot.refcount.store(0, Ordering::Release);
        slot.closed.store(false, Ordering::Release);
        self.free_head.lock(|h| {
            slot.next_free.store(h.get(), Ordering::Release);
            h.set(idx + 1);
        });
    }
}

// ── Bounded MPSC handles ──────────────────────────────────────────────

/// Bounded sender backed by a [`MpscPool`]. `Clone` increments the
/// slot's sender refcount; the receiver's `recv()` resolves to `None`
/// only after every clone (and the original) has been dropped.
pub struct StaticBoundedSender<T: Send + 'static, const SLOT_CAP: usize> {
    slot: &'static MpscSlot<T, SLOT_CAP>,
    pool: &'static dyn MpscReclaim<T, SLOT_CAP>,
}

impl<T: Send + 'static, const SLOT_CAP: usize> Clone for StaticBoundedSender<T, SLOT_CAP> {
    fn clone(&self) -> Self {
        self.slot.refcount.fetch_add(1, Ordering::AcqRel);
        Self {
            slot: self.slot,
            pool: self.pool,
        }
    }
}

impl<T: Send + 'static, const SLOT_CAP: usize> Drop for StaticBoundedSender<T, SLOT_CAP> {
    fn drop(&mut self) {
        // If we are the last sender (and receiver is alive — i.e.
        // refcount goes from 2→1 with the receiver-bit being the
        // remaining one), set closed + wake.
        let prev = self.slot.refcount.fetch_sub(1, Ordering::AcqRel);
        if prev == 2 {
            // Could be either "last sender, receiver alive" (we want
            // to close+wake) or "last receiver, sender alive" (no
            // close/wake — that's the receiver's drop). To
            // distinguish, set closed before decrementing? Simpler:
            // set closed unconditionally here. If the receiver was
            // the one that just dropped, `closed` is meaningless —
            // the slot will be reclaimed when refcount hits 0.
            self.slot.closed.store(true, Ordering::Release);
            self.slot.close_waker.wake();
        } else if prev == 1 {
            self.pool.release(self.slot);
        }
    }
}

impl<T: Send + 'static, const SLOT_CAP: usize> MpscSend<T> for StaticBoundedSender<T, SLOT_CAP> {
    async fn send(&self, value: T) -> Result<(), ()> {
        let slot = self.slot;
        // Fast path: receiver already gone.
        if slot.closed.load(Ordering::Acquire) {
            return Err(());
        }
        // Pin the embassy SendFuture on the stack so it survives
        // across yields without losing the captured value. Race it
        // against the closed flag via send_wakers.
        let mut send_fut = core::pin::pin!(slot.chan.send(value));
        poll_fn(|cx| {
            // If the receiver is already closed, report Err(()). A
            // send that polls Ready before the closed check returns
            // Ok(()), even if close happened concurrently after the
            // pre-poll check.
            if slot.closed.load(Ordering::Acquire) {
                return Poll::Ready(Err(()));
            }
            match send_fut.as_mut().poll(cx) {
                Poll::Ready(()) => Poll::Ready(Ok(())),
                Poll::Pending => {
                    // Register on send_wakers so a receiver drop wakes
                    // *all* awaiting senders, not just the most-recent.
                    // The embassy SendFuture has separately registered
                    // on the channel's internal waker.
                    slot.send_wakers
                        .lock(|w| w.borrow_mut().register(cx.waker()));
                    // Re-check closed after registering, to close the
                    // lost-wakeup window.
                    if slot.closed.load(Ordering::Acquire) {
                        return Poll::Ready(Err(()));
                    }
                    Poll::Pending
                }
            }
        })
        .await
    }
}

/// Bounded receiver backed by a [`MpscPool`].
pub struct StaticBoundedReceiver<T: Send + 'static, const SLOT_CAP: usize> {
    slot: &'static MpscSlot<T, SLOT_CAP>,
    pool: &'static dyn MpscReclaim<T, SLOT_CAP>,
}

impl<T: Send + 'static, const SLOT_CAP: usize> Drop for StaticBoundedReceiver<T, SLOT_CAP> {
    fn drop(&mut self) {
        // Receiver gone — mark closed and wake every pending sender
        // that's awaiting on a full channel. The send-side poll_fn
        // races the wake against the closed flag and observes Err.
        // Multi-waker so cloned senders are all woken, not just the
        // most-recently-registered one.
        self.slot.closed.store(true, Ordering::Release);
        self.slot.send_wakers.lock(|w| w.borrow_mut().wake());
        let prev = self.slot.refcount.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 {
            self.pool.release(self.slot);
        }
    }
}

impl<T: Send + 'static, const SLOT_CAP: usize> MpscRecv<T> for StaticBoundedReceiver<T, SLOT_CAP> {
    fn recv(&mut self) -> impl Future<Output = Option<T>> + Send + '_ {
        let slot = self.slot;
        async move { mpsc_recv_inner(slot).await }
    }

    fn poll_recv(&mut self, cx: &mut core::task::Context<'_>) -> core::task::Poll<Option<T>> {
        mpsc_poll_recv(self.slot, cx)
    }
}

// ── Unbounded MPSC handles ────────────────────────────────────────────

/// Unbounded sender — `send_now` returns `Err(value)` on a full slot
/// rather than blocking. Pool sizing must be generous enough that the
/// fixed-capacity slot is effectively unbounded for the workload; the
/// crate's existing Tokio path uses 128 as the default.
pub struct StaticUnboundedSender<T: Send + 'static, const SLOT_CAP: usize> {
    slot: &'static MpscSlot<T, SLOT_CAP>,
    pool: &'static dyn MpscReclaim<T, SLOT_CAP>,
}

impl<T: Send + 'static, const SLOT_CAP: usize> Clone for StaticUnboundedSender<T, SLOT_CAP> {
    fn clone(&self) -> Self {
        self.slot.refcount.fetch_add(1, Ordering::AcqRel);
        Self {
            slot: self.slot,
            pool: self.pool,
        }
    }
}

impl<T: Send + 'static, const SLOT_CAP: usize> Drop for StaticUnboundedSender<T, SLOT_CAP> {
    fn drop(&mut self) {
        let prev = self.slot.refcount.fetch_sub(1, Ordering::AcqRel);
        if prev == 2 {
            self.slot.closed.store(true, Ordering::Release);
            self.slot.close_waker.wake();
        } else if prev == 1 {
            self.pool.release(self.slot);
        }
    }
}

impl<T: Send + 'static, const SLOT_CAP: usize> UnboundedSend<T>
    for StaticUnboundedSender<T, SLOT_CAP>
{
    fn send_now(&self, value: T) -> Result<(), T> {
        // Refuse to push into a slot whose receiver has dropped, AND
        // reject `Full` from the underlying channel. The trait's
        // unified `Result<(), T>` does not distinguish "closed" from
        // "full" — callers that need to retry on transient fullness
        // should size `SLOT_CAP` so they do not happen, since the
        // unbounded sender only differs from the bounded one in its
        // non-await contract; both can fail with `Err(value)` here.
        if self.slot.closed.load(Ordering::Acquire) {
            return Err(value);
        }
        self.slot.chan.try_send(value).map_err(|e| match e {
            embassy_sync::channel::TrySendError::Full(v) => v,
        })
    }
}

/// Unbounded receiver.
pub struct StaticUnboundedReceiver<T: Send + 'static, const SLOT_CAP: usize> {
    slot: &'static MpscSlot<T, SLOT_CAP>,
    pool: &'static dyn MpscReclaim<T, SLOT_CAP>,
}

impl<T: Send + 'static, const SLOT_CAP: usize> Drop for StaticUnboundedReceiver<T, SLOT_CAP> {
    fn drop(&mut self) {
        self.slot.closed.store(true, Ordering::Release);
        // Unbounded send_now never awaits, but we still wake
        // send_wakers so any bounded sender on a slot that was reused
        // for unbounded duty observes the close. Cheap and safe.
        self.slot.send_wakers.lock(|w| w.borrow_mut().wake());
        let prev = self.slot.refcount.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 {
            self.pool.release(self.slot);
        }
    }
}

impl<T: Send + 'static, const SLOT_CAP: usize> UnboundedRecv<T>
    for StaticUnboundedReceiver<T, SLOT_CAP>
{
    fn recv(&mut self) -> impl Future<Output = Option<T>> + Send + '_ {
        let slot = self.slot;
        async move { mpsc_recv_inner(slot).await }
    }
}

// ── Shared MPSC recv plumbing ─────────────────────────────────────────

async fn mpsc_recv_inner<T: Send + 'static, const SLOT_CAP: usize>(
    slot: &'static MpscSlot<T, SLOT_CAP>,
) -> Option<T> {
    poll_fn(|cx| mpsc_poll_recv(slot, cx)).await
}

fn mpsc_poll_recv<T: Send + 'static, const SLOT_CAP: usize>(
    slot: &'static MpscSlot<T, SLOT_CAP>,
    cx: &mut core::task::Context<'_>,
) -> core::task::Poll<Option<T>> {
    if let Ok(v) = slot.chan.try_receive() {
        return Poll::Ready(Some(v));
    }
    if slot.closed.load(Ordering::Acquire) {
        // Drain race: a sender may have pushed a final value
        // concurrently with closing.
        if let Ok(v) = slot.chan.try_receive() {
            return Poll::Ready(Some(v));
        }
        return Poll::Ready(None);
    }
    slot.close_waker.register(cx.waker());
    {
        let mut fut = slot.chan.receive();
        // SAFETY: `fut` is stack-pinned, polled once, then dropped.
        let pinned = unsafe { Pin::new_unchecked(&mut fut) };
        if let Poll::Ready(v) = pinned.poll(cx) {
            return Poll::Ready(Some(v));
        }
    }
    if let Ok(v) = slot.chan.try_receive() {
        return Poll::Ready(Some(v));
    }
    if slot.closed.load(Ordering::Acquire) {
        if let Ok(v) = slot.chan.try_receive() {
            return Poll::Ready(Some(v));
        }
        return Poll::Ready(None);
    }
    Poll::Pending
}

// ── Debug impls ───────────────────────────────────────────────────────

impl<T: Send + 'static> core::fmt::Debug for OneshotSlot<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OneshotSlot")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl<T: Send + 'static, const N: usize> core::fmt::Debug for OneshotPool<T, N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OneshotPool").finish_non_exhaustive()
    }
}

impl<T: Send + 'static> core::fmt::Debug for StaticOneshotSender<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StaticOneshotSender")
            .field("sent", &self.sent)
            .finish_non_exhaustive()
    }
}

impl<T: Send + 'static> core::fmt::Debug for StaticOneshotReceiver<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StaticOneshotReceiver")
            .finish_non_exhaustive()
    }
}

impl<T: Send + 'static, const N: usize> core::fmt::Debug for MpscSlot<T, N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MpscSlot")
            .field("refcount", &self.refcount)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

impl<T: Send + 'static, const P: usize, const N: usize> core::fmt::Debug for MpscPool<T, P, N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MpscPool").finish_non_exhaustive()
    }
}

impl<T: Send + 'static, const N: usize> core::fmt::Debug for StaticBoundedSender<T, N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StaticBoundedSender")
            .finish_non_exhaustive()
    }
}

impl<T: Send + 'static, const N: usize> core::fmt::Debug for StaticBoundedReceiver<T, N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StaticBoundedReceiver")
            .finish_non_exhaustive()
    }
}

impl<T: Send + 'static, const N: usize> core::fmt::Debug for StaticUnboundedSender<T, N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StaticUnboundedSender")
            .finish_non_exhaustive()
    }
}

impl<T: Send + 'static, const N: usize> core::fmt::Debug for StaticUnboundedReceiver<T, N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StaticUnboundedReceiver")
            .finish_non_exhaustive()
    }
}

// ── `define_static_channels!` macro ───────────────────────────────────

/// Default slot capacity for unbounded channels declared via
/// [`define_static_channels!`](crate::define_static_channels). Matches the value used by the
/// embassy-sync-backed `EmbassySyncChannels::unbounded`. Each
/// unbounded `T` declared in the macro gets its own `MpscPool`
/// sized at `pool_size × UNBOUNDED_DEFAULT_CAP`.
pub const UNBOUNDED_DEFAULT_CAP: usize = 128;

/// Generates a no-alloc [`ChannelFactory`] from a user-authored pool
/// layout.
///
/// [`ChannelFactory`]: crate::transport::ChannelFactory
///
/// The macro emits:
/// - A unit struct `pub struct $name;` implementing
///   [`ChannelFactory`] with associated types pointing at this
///   module's [`StaticOneshotSender`] / `StaticBoundedSender` /
///   `StaticUnboundedSender` (and matching receivers).
/// - One `impl OneshotPooled<$name> for T` per `oneshot` entry,
///   wrapping a function-local `static OneshotPool<T, POOL_SIZE>`.
/// - One `impl BoundedPooled<$name, SLOT_CAP> for T` per `bounded`
///   entry.
/// - One `impl UnboundedPooled<$name> for T` per `unbounded` entry,
///   each backed by an `MpscPool<T, POOL_SIZE,
///   UNBOUNDED_DEFAULT_CAP>`.
///
/// Pool exhaustion in the generated `*_pair()` impls is reported
/// via `expect()` (see module-level docs).
///
/// # Example
///
/// ```ignore
/// use simple_someip::define_static_channels;
///
/// define_static_channels! {
///     name: MyChannels,
///     oneshot: [
///         (Result<(), MyError>, 80),
///         (RebootResponse, 4),
///     ],
///     bounded: [
///         ((ControlMessage<P, MyChannels>, 4), 1),
///         ((SendMessage<P, MyChannels>, 16), 8),
///     ],
///     unbounded: [
///         (ClientUpdate<P>, 1),
///     ],
/// }
/// ```
///
/// All three sections are required; pass an empty `[]` if a family
/// has no entries. The bounded entry shape is
/// `((Type, slot_cap), pool_size)` to disambiguate the slot cap
/// from the pool size in the macro grammar.
#[macro_export]
macro_rules! define_static_channels {
    // Entry point: explicit visibility.
    ( vis: $vis:vis, name: $name:ident, $($rest:tt)* ) => {
        $crate::define_static_channels! { @body $vis, $name, $($rest)* }
    };
    // Entry point: no visibility token — default to `pub`.
    ( name: $name:ident, $($rest:tt)* ) => {
        $crate::define_static_channels! { @body pub, $name, $($rest)* }
    };
    (
        @body $vis:vis, $name:ident,
        oneshot: [ $( ($ot:ty, $opool:literal) ),* $(,)? ],
        bounded: [ $( (($bt:ty, $bcap:literal), $bpool:literal) ),* $(,)? ],
        unbounded: [ $( ($ut:ty, $upool:literal) ),* $(,)? ] $(,)?
    ) => {
        #[derive(Clone, Copy, Debug)]
        $vis struct $name;

        impl $crate::transport::ChannelFactory for $name {
            type OneshotSender<T: ::core::marker::Send + 'static> =
                $crate::static_channels::StaticOneshotSender<T>;
            type OneshotReceiver<T: ::core::marker::Send + 'static> =
                $crate::static_channels::StaticOneshotReceiver<T>;
            type BoundedSender<T: ::core::marker::Send + 'static, const N: usize> =
                $crate::static_channels::StaticBoundedSender<T, N>;
            type BoundedReceiver<T: ::core::marker::Send + 'static, const N: usize> =
                $crate::static_channels::StaticBoundedReceiver<T, N>;
            type UnboundedSender<T: ::core::marker::Send + 'static> =
                $crate::static_channels::StaticUnboundedSender<
                    T,
                    { $crate::static_channels::UNBOUNDED_DEFAULT_CAP },
                >;
            type UnboundedReceiver<T: ::core::marker::Send + 'static> =
                $crate::static_channels::StaticUnboundedReceiver<
                    T,
                    { $crate::static_channels::UNBOUNDED_DEFAULT_CAP },
                >;
        }

        $(
            impl $crate::transport::OneshotPooled<$name> for $ot {
                fn oneshot_pair() -> (
                    <$name as $crate::transport::ChannelFactory>::OneshotSender<Self>,
                    <$name as $crate::transport::ChannelFactory>::OneshotReceiver<Self>,
                ) {
                    static POOL: $crate::static_channels::OneshotPool<$ot, $opool> =
                        $crate::static_channels::OneshotPool::new();
                    POOL.claim().expect(::core::concat!(
                        "OneshotPool<",
                        ::core::stringify!($ot),
                        ", ",
                        ::core::stringify!($opool),
                        "> exhausted; increase the pool size declared in define_static_channels!"
                    ))
                }
            }
        )*

        $(
            impl $crate::transport::BoundedPooled<$name, $bcap> for $bt {
                fn bounded_pair() -> (
                    <$name as $crate::transport::ChannelFactory>::BoundedSender<Self, $bcap>,
                    <$name as $crate::transport::ChannelFactory>::BoundedReceiver<Self, $bcap>,
                ) {
                    static POOL: $crate::static_channels::MpscPool<$bt, $bpool, $bcap> =
                        $crate::static_channels::MpscPool::new();
                    POOL.claim_bounded().expect(::core::concat!(
                        "MpscPool<",
                        ::core::stringify!($bt),
                        ", pool=",
                        ::core::stringify!($bpool),
                        ", slot_cap=",
                        ::core::stringify!($bcap),
                        "> exhausted; increase the pool size declared in define_static_channels!"
                    ))
                }
            }
        )*

        $(
            impl $crate::transport::UnboundedPooled<$name> for $ut {
                fn unbounded_pair() -> (
                    <$name as $crate::transport::ChannelFactory>::UnboundedSender<Self>,
                    <$name as $crate::transport::ChannelFactory>::UnboundedReceiver<Self>,
                ) {
                    static POOL: $crate::static_channels::MpscPool<
                        $ut,
                        $upool,
                        { $crate::static_channels::UNBOUNDED_DEFAULT_CAP },
                    > = $crate::static_channels::MpscPool::new();
                    POOL.claim_unbounded().expect(::core::concat!(
                        "MpscPool<",
                        ::core::stringify!($ut),
                        ", pool=",
                        ::core::stringify!($upool),
                        ", unbounded> exhausted; increase the pool size declared in define_static_channels!"
                    ))
                }
            }
        )*
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};
    use std::boxed::Box;

    fn poll_once<F: Future>(f: &mut core::pin::Pin<&mut F>) -> Poll<F::Output> {
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        f.as_mut().poll(&mut cx)
    }

    // ── Oneshot tests ─────────────────────────────────────────────────

    static ONESHOT_POOL_4: OneshotPool<u32, 4> = OneshotPool::new();

    #[test]
    fn oneshot_send_recv_happy_path() {
        let (tx, rx) = ONESHOT_POOL_4.claim().expect("pool not empty");
        tx.send(42).unwrap();
        let mut fut = pin!(rx.recv());
        match poll_once(&mut fut) {
            Poll::Ready(Ok(v)) => assert_eq!(v, 42),
            other => panic!("expected ready ok, got {other:?}"),
        }
    }

    #[test]
    fn oneshot_sender_drop_cancels_receiver() {
        let (tx, rx) = ONESHOT_POOL_4.claim().expect("pool not empty");
        drop(tx);
        let mut fut = pin!(rx.recv());
        match poll_once(&mut fut) {
            Poll::Ready(Err(OneshotCancelled)) => {}
            other => panic!("expected cancelled, got {other:?}"),
        }
    }

    #[test]
    fn oneshot_claim_release_cycles() {
        static POOL: OneshotPool<u32, 4> = OneshotPool::new();
        // Claim all 4, verify pool is exhausted, drop, re-claim.
        let p1 = POOL.claim().unwrap();
        let p2 = POOL.claim().unwrap();
        let p3 = POOL.claim().unwrap();
        let p4 = POOL.claim().unwrap();
        assert!(POOL.claim().is_none(), "5th claim must exhaust");
        drop((p1, p2, p3, p4));
        let p5 = POOL.claim();
        assert!(p5.is_some(), "post-drop claim must succeed");
    }

    #[test]
    fn oneshot_pool_exhaustion_returns_none() {
        static POOL_2: OneshotPool<u32, 2> = OneshotPool::new();
        let _a = POOL_2.claim().unwrap();
        let _b = POOL_2.claim().unwrap();
        assert!(POOL_2.claim().is_none(), "third claim must exhaust");
    }

    /// Concurrent first-claim: two threads call `claim()` on the same
    /// freshly-`new()`'d pool simultaneously. Both must succeed (the
    /// pool has 8 slots). Regression for the seeding race where one
    /// thread won the CAS and started looping while the other took
    /// `free_head` first and observed `head == 0`.
    #[test]
    fn oneshot_concurrent_first_claim_does_not_panic() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering as O};
        static POOL: OneshotPool<u32, 8> = OneshotPool::new();
        let success_count = Arc::new(AtomicUsize::new(0));
        let mut handles = std::vec::Vec::new();
        for _ in 0..4 {
            let s = Arc::clone(&success_count);
            handles.push(std::thread::spawn(move || {
                if POOL.claim().is_some() {
                    s.fetch_add(1, O::SeqCst);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            success_count.load(O::SeqCst),
            4,
            "all 4 concurrent claims should have succeeded against an 8-slot pool",
        );
    }

    /// Multi-sender close broadcast: when the receiver drops, every
    /// cloned sender that is awaiting a full-channel `send` must
    /// resolve to `Err(())`. Regression for the old single-slot
    /// `AtomicWaker` which only woke the most-recently-registered
    /// sender.
    #[test]
    fn mpsc_bounded_receiver_drop_wakes_all_cloned_senders() {
        static POOL: MpscPool<u32, 4, 1> = MpscPool::new();
        let (tx, rx) = POOL.claim_bounded().expect("claim");
        // Fill the channel so any further send awaits.
        let mut filler_fut = pin!(tx.send(0));
        match poll_once(&mut filler_fut) {
            Poll::Ready(Ok(())) => {}
            other => panic!("filler send should resolve immediately: {other:?}"),
        }
        // Three cloned senders, all awaiting on the full channel.
        let clones: std::vec::Vec<_> = (0..3).map(|_| tx.clone()).collect();
        let mut futs: std::vec::Vec<_> = clones
            .iter()
            .enumerate()
            .map(|(i, c)| Box::pin(c.send(u32::try_from(i).unwrap() + 1)))
            .collect();
        for f in &mut futs {
            // Each should park (channel is full).
            match f.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
                Poll::Pending => {}
                Poll::Ready(other) => panic!("expected Pending, got Ready({other:?})"),
            }
        }
        drop(rx);
        // Each cloned sender's pending future must now resolve to Err.
        for f in &mut futs {
            match f.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
                Poll::Ready(Err(())) => {}
                Poll::Ready(Ok(())) => {
                    panic!("expected Err after receiver drop on cloned sender, got Ok")
                }
                Poll::Pending => panic!("expected Err after receiver drop, got Pending"),
            }
        }
    }

    #[test]
    fn mpsc_concurrent_first_claim_does_not_panic() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering as O};
        static POOL: MpscPool<u32, 8, 4> = MpscPool::new();
        let success_count = Arc::new(AtomicUsize::new(0));
        let mut handles = std::vec::Vec::new();
        for _ in 0..4 {
            let s = Arc::clone(&success_count);
            handles.push(std::thread::spawn(move || {
                if POOL.claim_bounded().is_some() {
                    s.fetch_add(1, O::SeqCst);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            success_count.load(O::SeqCst),
            4,
            "all 4 concurrent claims should have succeeded against an 8-slot pool",
        );
    }

    // ── Bounded MPSC tests ────────────────────────────────────────────

    static MPSC_POOL: MpscPool<u32, 2, 4> = MpscPool::new();

    #[test]
    fn mpsc_bounded_send_recv() {
        let (tx, mut rx) = MPSC_POOL.claim_bounded().expect("pool not empty");
        let mut send_fut = pin!(tx.send(7));
        assert!(matches!(poll_once(&mut send_fut), Poll::Ready(Ok(()))));
        let mut recv_fut = pin!(rx.recv());
        match poll_once(&mut recv_fut) {
            Poll::Ready(Some(7)) => {}
            other => panic!("expected ready Some(7), got {other:?}"),
        }
    }

    #[test]
    fn mpsc_bounded_clone_then_drop_all_closes_receiver() {
        static POOL: MpscPool<u32, 1, 2> = MpscPool::new();
        let (tx, mut rx) = POOL.claim_bounded().expect("pool not empty");
        let tx2 = tx.clone();
        drop(tx);
        // One clone still alive — receiver should not be closed yet.
        {
            let mut recv_fut = pin!(rx.recv());
            assert!(matches!(poll_once(&mut recv_fut), Poll::Pending));
        }
        drop(tx2);
        // All senders gone → receiver resolves to None.
        let mut recv_fut = pin!(rx.recv());
        match poll_once(&mut recv_fut) {
            Poll::Ready(None) => {}
            other => panic!("expected ready None, got {other:?}"),
        }
    }

    // ── Unbounded MPSC tests ──────────────────────────────────────────

    #[test]
    fn unbounded_send_now_returns_full_when_capacity_exhausted() {
        static POOL: MpscPool<u32, 1, 2> = MpscPool::new();
        let (tx, _rx) = POOL.claim_unbounded().expect("pool not empty");
        assert!(tx.send_now(1).is_ok());
        assert!(tx.send_now(2).is_ok());
        match tx.send_now(3) {
            Err(3) => {}
            other => panic!("expected Err(3), got {other:?}"),
        }
    }

    // ── define_static_channels! macro ─────────────────────────────────

    // Witness that the macro expands to a `ChannelFactory` with all
    // three families wired and that the per-`T` `*Pooled` impls
    // dispatch correctly.
    crate::define_static_channels! {
        name: MacroTestChannels,
        oneshot: [
            (u32, 4),
            (Result<i32, ()>, 2),
        ],
        bounded: [
            ((u8, 4), 2),
        ],
        unbounded: [
            (u16, 1),
        ],
    }

    #[test]
    fn macro_oneshot_dispatches_through_factory() {
        use crate::transport::{ChannelFactory, OneshotSend};
        let (tx, rx) = MacroTestChannels::oneshot::<u32>();
        tx.send(99).unwrap();
        let mut fut = pin!(<_ as crate::transport::OneshotRecv<u32>>::recv(rx));
        match poll_once(&mut fut) {
            Poll::Ready(Ok(99)) => {}
            other => panic!("expected ready Ok(99), got {other:?}"),
        }
    }

    #[test]
    fn macro_bounded_dispatches_through_factory() {
        use crate::transport::{ChannelFactory, MpscRecv, MpscSend};
        let (tx, mut rx) = MacroTestChannels::bounded::<u8, 4>();
        {
            let mut send_fut = pin!(tx.send(7));
            assert!(matches!(poll_once(&mut send_fut), Poll::Ready(Ok(()))));
        }
        let mut recv_fut = pin!(rx.recv());
        match poll_once(&mut recv_fut) {
            Poll::Ready(Some(7)) => {}
            other => panic!("expected ready Some(7), got {other:?}"),
        }
    }

    #[test]
    fn macro_unbounded_dispatches_through_factory() {
        use crate::transport::{ChannelFactory, UnboundedSend};
        let (tx, _rx) = MacroTestChannels::unbounded::<u16>();
        assert!(tx.send_now(1234).is_ok());
    }

    // ── Waker-tracking helper ─────────────────────────────────────────

    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering as SAtomic};

    struct WakeFlag(AtomicBool);
    impl std::task::Wake for WakeFlag {
        fn wake(self: Arc<Self>) {
            self.0.store(true, SAtomic::Release);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.store(true, SAtomic::Release);
        }
    }
    fn tracking_waker() -> (Arc<WakeFlag>, Waker) {
        let flag = Arc::new(WakeFlag(AtomicBool::new(false)));
        let waker = Waker::from(flag.clone());
        (flag, waker)
    }

    // ── Waker firing tests ────────────────────────────────────────────

    #[test]
    fn oneshot_waker_fires_on_send() {
        static POOL: OneshotPool<u32, 2> = OneshotPool::new();
        let (tx, rx) = POOL.claim().expect("pool not empty");
        let (flag, waker) = tracking_waker();
        let mut cx = Context::from_waker(&waker);
        let mut fut = pin!(rx.recv());
        assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Pending));
        tx.send(42u32).unwrap();
        assert!(
            flag.0.load(SAtomic::Acquire),
            "waker must fire when value is sent"
        );
        let noop = Waker::noop();
        let mut cx2 = Context::from_waker(noop);
        assert!(matches!(fut.as_mut().poll(&mut cx2), Poll::Ready(Ok(42))));
    }

    #[test]
    fn oneshot_cancel_waker_fires_on_sender_drop() {
        static POOL: OneshotPool<u32, 2> = OneshotPool::new();
        let (tx, rx) = POOL.claim().expect("pool not empty");
        let (flag, waker) = tracking_waker();
        let mut cx = Context::from_waker(&waker);
        let mut fut = pin!(rx.recv());
        assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Pending));
        drop(tx);
        assert!(
            flag.0.load(SAtomic::Acquire),
            "waker must fire when sender is dropped (cancel)"
        );
        let noop = Waker::noop();
        let mut cx2 = Context::from_waker(noop);
        assert!(matches!(
            fut.as_mut().poll(&mut cx2),
            Poll::Ready(Err(OneshotCancelled))
        ));
    }

    #[test]
    fn mpsc_close_waker_fires_on_all_senders_drop() {
        static POOL: MpscPool<u32, 1, 4> = MpscPool::new();
        let (tx, mut rx) = POOL.claim_bounded().expect("pool not empty");
        let tx2 = tx.clone();
        let (flag, waker) = tracking_waker();
        let mut cx = Context::from_waker(&waker);
        let mut fut = pin!(rx.recv());
        assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Pending));
        drop(tx);
        assert!(
            !flag.0.load(SAtomic::Acquire),
            "waker must not fire until last sender drops"
        );
        drop(tx2);
        assert!(
            flag.0.load(SAtomic::Acquire),
            "waker must fire when last sender drops"
        );
        let noop = Waker::noop();
        let mut cx2 = Context::from_waker(noop);
        assert!(matches!(fut.as_mut().poll(&mut cx2), Poll::Ready(None)));
    }

    #[test]
    fn mpsc_bounded_pool_exhaustion_returns_none() {
        static POOL: MpscPool<u32, 1, 4> = MpscPool::new();
        let _a = POOL.claim_bounded().expect("pool not empty");
        assert!(
            POOL.claim_bounded().is_none(),
            "second claim must exhaust pool of size 1"
        );
    }

    // ── Sender-side close-semantic tests ──────────────────────────────

    #[test]
    fn oneshot_send_after_receiver_drop_returns_err() {
        static POOL: OneshotPool<u32, 2> = OneshotPool::new();
        let (tx, rx) = POOL.claim().expect("pool not empty");
        drop(rx);
        match tx.send(42) {
            Err(42) => {}
            other => panic!("expected Err(42) after receiver drop, got {other:?}"),
        }
    }

    #[test]
    fn unbounded_send_now_after_receiver_drop_returns_err() {
        static POOL: MpscPool<u32, 1, 4> = MpscPool::new();
        let (tx, rx) = POOL.claim_unbounded().expect("pool not empty");
        drop(rx);
        match tx.send_now(7) {
            Err(7) => {}
            other => panic!("expected Err(7) after receiver drop, got {other:?}"),
        }
    }

    #[test]
    fn bounded_send_unblocks_with_err_on_receiver_drop() {
        static POOL: MpscPool<u32, 1, 1> = MpscPool::new();
        let (tx, rx) = POOL.claim_bounded().expect("pool not empty");
        // Capacity is 1; fill it.
        {
            let mut send_fut = pin!(tx.send(1));
            assert!(matches!(poll_once(&mut send_fut), Poll::Ready(Ok(()))));
        }
        // Next send must wait — channel is full.
        let mut send_fut = pin!(tx.send(2));
        let (flag, waker) = tracking_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(send_fut.as_mut().poll(&mut cx), Poll::Pending));
        // Drop the receiver — sender's send_waker must fire and the
        // next poll must return Err(()).
        drop(rx);
        assert!(
            flag.0.load(SAtomic::Acquire),
            "send_waker must fire when receiver drops while sender is awaiting"
        );
        let noop = Waker::noop();
        let mut cx2 = Context::from_waker(noop);
        match send_fut.as_mut().poll(&mut cx2) {
            Poll::Ready(Err(())) => {}
            other => panic!("expected Err(()) after receiver drop, got {other:?}"),
        }
    }

    #[test]
    fn bounded_send_after_receiver_drop_returns_err_fast_path() {
        static POOL: MpscPool<u32, 1, 4> = MpscPool::new();
        let (tx, rx) = POOL.claim_bounded().expect("pool not empty");
        drop(rx);
        let mut send_fut = pin!(tx.send(99));
        match poll_once(&mut send_fut) {
            Poll::Ready(Err(())) => {}
            other => panic!("expected Err(()) on closed slot, got {other:?}"),
        }
    }
}
