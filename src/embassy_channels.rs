//! [`ChannelFactory`] backed by `embassy-sync::channel::Channel`. Active
//! when the `embassy_channels` feature is enabled.
//!
//! # Heap allocation per call
//!
//! Both sender and receiver hold an `Arc<Inner<...>>`, and every
//! call to [`EmbassySyncChannels::oneshot()`][of], [`bounded()`][bf], or
//! [`unbounded()`][uf] heap-allocates a fresh `Arc<Inner<...>>`. The
//! `Client` run-loop calls these per request-response pair — most
//! notably, every method on `Client` that awaits a server response
//! constructs a oneshot via this factory, so each such method
//! triggers one `Arc` allocation.
//!
//! [of]: crate::transport::ChannelFactory::oneshot
//! [bf]: crate::transport::ChannelFactory::bounded
//! [uf]: crate::transport::ChannelFactory::unbounded
//!
//! # Use [`crate::static_channels`] for the no-alloc bare-metal path
//!
//! [`crate::static_channels`] ships a no-alloc `ChannelFactory` whose
//! senders and receivers carry `&'static` references into pre-allocated
//! [`OneshotPool`] / [`MpscPool`] storage. The
//! [`define_static_channels!`][dsc] macro generates the per-`T`
//! `*Pooled<MyChannels>` impls + a [`ChannelFactory`] impl on a unit
//! struct.
//!
//! [`OneshotPool`]: crate::static_channels::OneshotPool
//! [`MpscPool`]: crate::static_channels::MpscPool
//! [dsc]: crate::define_static_channels
//!
//! `EmbassySyncChannels` remains useful for two cases:
//!
//! 1. Bringing up a bare-metal port on `std + alloc` targets where
//!    you want the trait-surface integration validated before
//!    declaring static pool sizes.
//! 2. Demonstrating the `ChannelFactory` integration shape for
//!    consumers writing their own backend.
//!
//! For production firmware targeting "zero heap after
//! `Client::new` returns", switch to the macro-declared static
//! pools.
//!
//! # Close semantics
//!
//! All six channel families honor the close contracts in
//! [`crate::transport`]:
//!
//! - **Oneshot**: sender drop without `send` resolves the receiver's
//!   `recv()` to `Err(OneshotCancelled)`. Receiver drop causes the
//!   sender's `send()` to return `Err(value)`.
//! - **Bounded MPSC**: when the receiver drops, any sender awaiting on
//!   a full channel is woken and returns `Err(())`. When the last
//!   sender drops, the receiver's `recv()` resolves to `None`.
//! - **Unbounded MPSC**: same close contracts as bounded. `send_now`
//!   returns `Err(value)` if either the channel is full or the
//!   receiver has dropped.
//!
//! Multi-sender contention on a closed bounded channel: the close
//! signal uses a single `AtomicWaker`, so only the most-recent
//! sender to register wakes immediately on receiver drop. Other
//! awaiting senders will eventually re-poll (e.g. when the embassy
//! channel's internal waker fires) and observe the closed flag —
//! convergent but not constant-latency.

use alloc::sync::Arc;
use core::future::{Future, poll_fn};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use core::task::Poll;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::waitqueue::AtomicWaker;

use crate::transport::{
    BoundedPooled, ChannelFactory, MpscRecv, MpscSend, OneshotCancelled, OneshotPooled,
    OneshotRecv, OneshotSend, UnboundedPooled, UnboundedRecv, UnboundedSend,
};

// ── Oneshot (capacity-1 Channel) ──────────────────────────────────────

struct OneshotInner<T: Send + 'static> {
    chan: Channel<CriticalSectionRawMutex, T, 1>,
    /// Cleared when the sender drops without sending; receiver's
    /// `recv()` then resolves to `Err(OneshotCancelled)`.
    sender_alive: AtomicBool,
    /// Cleared when the receiver drops; sender's `send()` then
    /// returns `Err(value)`.
    receiver_alive: AtomicBool,
    /// Wakes the receiver when the sender drops without sending.
    cancel_waker: AtomicWaker,
}

impl<T: Send + 'static> OneshotInner<T> {
    fn new() -> Self {
        Self {
            chan: Channel::new(),
            sender_alive: AtomicBool::new(true),
            receiver_alive: AtomicBool::new(true),
            cancel_waker: AtomicWaker::new(),
        }
    }
}

pub struct EmbassySyncOneshotSender<T: Send + 'static> {
    inner: Arc<OneshotInner<T>>,
    sent: bool,
}

pub struct EmbassySyncOneshotReceiver<T: Send + 'static> {
    inner: Arc<OneshotInner<T>>,
}

impl<T: Send + 'static> OneshotSend<T> for EmbassySyncOneshotSender<T> {
    fn send(mut self, value: T) -> Result<(), T> {
        if !self.inner.receiver_alive.load(Ordering::Acquire) {
            return Err(value);
        }
        match self.inner.chan.try_send(value) {
            Ok(()) => {
                self.sent = true;
                Ok(())
            }
            Err(embassy_sync::channel::TrySendError::Full(v)) => Err(v),
        }
    }
}

impl<T: Send + 'static> Drop for EmbassySyncOneshotSender<T> {
    fn drop(&mut self) {
        if !self.sent {
            self.inner.sender_alive.store(false, Ordering::Release);
            self.inner.cancel_waker.wake();
        }
    }
}

impl<T: Send + 'static> OneshotRecv<T> for EmbassySyncOneshotReceiver<T> {
    // The complex `poll_fn` body with manual pinning requires an explicit
    // async block rather than `async fn` syntax.
    #[allow(clippy::manual_async_fn)]
    fn recv(self) -> impl Future<Output = Result<T, OneshotCancelled>> + Send {
        async move {
            let inner = &self.inner;
            poll_fn(move |cx| {
                if let Ok(v) = inner.chan.try_receive() {
                    return Poll::Ready(Ok(v));
                }
                if !inner.sender_alive.load(Ordering::Acquire) {
                    return Poll::Ready(Err(OneshotCancelled));
                }
                inner.cancel_waker.register(cx.waker());
                // Poll embassy's receive future to register on the
                // channel's internal waker.
                let mut fut = inner.chan.receive();
                // SAFETY: stack-pinned, polled once, dropped before
                // exiting this scope. No reference escapes.
                let pinned = unsafe { core::pin::Pin::new_unchecked(&mut fut) };
                if let Poll::Ready(v) = pinned.poll(cx) {
                    return Poll::Ready(Ok(v));
                }
                // Re-check both signals after registration to close
                // the lost-wakeup window.
                if let Ok(v) = inner.chan.try_receive() {
                    return Poll::Ready(Ok(v));
                }
                if !inner.sender_alive.load(Ordering::Acquire) {
                    return Poll::Ready(Err(OneshotCancelled));
                }
                Poll::Pending
            })
            .await
        }
    }
}

impl<T: Send + 'static> Drop for EmbassySyncOneshotReceiver<T> {
    fn drop(&mut self) {
        self.inner.receiver_alive.store(false, Ordering::Release);
    }
}

// ── MPSC Inner (shared by bounded + unbounded) ────────────────────────

struct MpscInner<T: Send + 'static, const N: usize> {
    chan: Channel<CriticalSectionRawMutex, T, N>,
    /// Number of live senders (sum of all clones).
    sender_count: AtomicUsize,
    /// `true` once either the receiver dropped or the last sender
    /// dropped. Senders observe this to short-circuit; receivers use
    /// it as the empty-and-done signal.
    closed: AtomicBool,
    /// Wakes the receiver when the last sender drops.
    recv_waker: AtomicWaker,
    /// Wakes a bounded sender awaiting on a full channel when the
    /// receiver drops. Single-slot — multi-sender contention is
    /// best-effort.
    send_waker: AtomicWaker,
}

impl<T: Send + 'static, const N: usize> MpscInner<T, N> {
    fn new() -> Self {
        Self {
            chan: Channel::new(),
            sender_count: AtomicUsize::new(1),
            closed: AtomicBool::new(false),
            recv_waker: AtomicWaker::new(),
            send_waker: AtomicWaker::new(),
        }
    }
}

// ── Bounded MPSC ──────────────────────────────────────────────────────

pub struct EmbassySyncBoundedSender<T: Send + 'static, const N: usize> {
    inner: Arc<MpscInner<T, N>>,
}

pub struct EmbassySyncBoundedReceiver<T: Send + 'static, const N: usize> {
    inner: Arc<MpscInner<T, N>>,
}

impl<T: Send + 'static, const N: usize> Clone for EmbassySyncBoundedSender<T, N> {
    fn clone(&self) -> Self {
        self.inner.sender_count.fetch_add(1, Ordering::AcqRel);
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T: Send + 'static, const N: usize> Drop for EmbassySyncBoundedSender<T, N> {
    fn drop(&mut self) {
        let prev = self.inner.sender_count.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 {
            // Last sender — close the channel and wake the receiver.
            self.inner.closed.store(true, Ordering::Release);
            self.inner.recv_waker.wake();
        }
    }
}

impl<T: Send + 'static, const N: usize> MpscSend<T> for EmbassySyncBoundedSender<T, N> {
    fn send(&self, value: T) -> impl Future<Output = Result<(), ()>> + Send + '_ {
        let inner = self.inner.clone();
        async move {
            if inner.closed.load(Ordering::Acquire) {
                drop(value);
                return Err(());
            }
            // Pin embassy's SendFuture on the stack so the captured
            // value survives across yields. Race against the closed
            // flag.
            let mut send_fut = core::pin::pin!(inner.chan.send(value));
            poll_fn(|cx| {
                if inner.closed.load(Ordering::Acquire) {
                    return Poll::Ready(Err(()));
                }
                match send_fut.as_mut().poll(cx) {
                    Poll::Ready(()) => Poll::Ready(Ok(())),
                    Poll::Pending => {
                        inner.send_waker.register(cx.waker());
                        if inner.closed.load(Ordering::Acquire) {
                            return Poll::Ready(Err(()));
                        }
                        Poll::Pending
                    }
                }
            })
            .await
        }
    }
}

impl<T: Send + 'static, const N: usize> Drop for EmbassySyncBoundedReceiver<T, N> {
    fn drop(&mut self) {
        // Receiver gone — mark closed and wake any awaiting sender.
        self.inner.closed.store(true, Ordering::Release);
        self.inner.send_waker.wake();
    }
}

impl<T: Send + 'static, const N: usize> MpscRecv<T> for EmbassySyncBoundedReceiver<T, N> {
    fn recv(&mut self) -> impl Future<Output = Option<T>> + Send + '_ {
        let inner = self.inner.clone();
        async move { mpsc_recv_inner(inner).await }
    }

    fn poll_recv(&mut self, cx: &mut core::task::Context<'_>) -> core::task::Poll<Option<T>> {
        mpsc_poll_recv(&self.inner, cx)
    }
}

// ── Unbounded MPSC ────────────────────────────────────────────────────

const UNBOUNDED_CAP: usize = 128;

pub struct EmbassySyncUnboundedSender<T: Send + 'static> {
    inner: Arc<MpscInner<T, UNBOUNDED_CAP>>,
}

pub struct EmbassySyncUnboundedReceiver<T: Send + 'static> {
    inner: Arc<MpscInner<T, UNBOUNDED_CAP>>,
}

impl<T: Send + 'static> Clone for EmbassySyncUnboundedSender<T> {
    fn clone(&self) -> Self {
        self.inner.sender_count.fetch_add(1, Ordering::AcqRel);
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T: Send + 'static> Drop for EmbassySyncUnboundedSender<T> {
    fn drop(&mut self) {
        let prev = self.inner.sender_count.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 {
            self.inner.closed.store(true, Ordering::Release);
            self.inner.recv_waker.wake();
        }
    }
}

impl<T: Send + 'static> UnboundedSend<T> for EmbassySyncUnboundedSender<T> {
    fn send_now(&self, value: T) -> Result<(), T> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(value);
        }
        self.inner.chan.try_send(value).map_err(|e| match e {
            embassy_sync::channel::TrySendError::Full(v) => v,
        })
    }
}

impl<T: Send + 'static> Drop for EmbassySyncUnboundedReceiver<T> {
    fn drop(&mut self) {
        self.inner.closed.store(true, Ordering::Release);
        self.inner.send_waker.wake();
    }
}

impl<T: Send + 'static> UnboundedRecv<T> for EmbassySyncUnboundedReceiver<T> {
    fn recv(&mut self) -> impl Future<Output = Option<T>> + Send + '_ {
        let inner = self.inner.clone();
        async move { mpsc_recv_inner(inner).await }
    }
}

// ── Shared MPSC recv plumbing ─────────────────────────────────────────

async fn mpsc_recv_inner<T: Send + 'static, const N: usize>(
    inner: Arc<MpscInner<T, N>>,
) -> Option<T> {
    poll_fn(move |cx| mpsc_poll_recv(&inner, cx)).await
}

fn mpsc_poll_recv<T: Send + 'static, const N: usize>(
    inner: &MpscInner<T, N>,
    cx: &mut core::task::Context<'_>,
) -> core::task::Poll<Option<T>> {
    if let Ok(v) = inner.chan.try_receive() {
        return Poll::Ready(Some(v));
    }
    if inner.closed.load(Ordering::Acquire) {
        if let Ok(v) = inner.chan.try_receive() {
            return Poll::Ready(Some(v));
        }
        return Poll::Ready(None);
    }
    inner.recv_waker.register(cx.waker());
    // Poll embassy's receive future to register on its internal
    // waker so per-value sends wake us.
    let mut fut = inner.chan.receive();
    // SAFETY: stack-pinned, polled once, dropped before this scope ends.
    let pinned = unsafe { core::pin::Pin::new_unchecked(&mut fut) };
    if let Poll::Ready(v) = pinned.poll(cx) {
        return Poll::Ready(Some(v));
    }
    // Re-check both signals after registration.
    if let Ok(v) = inner.chan.try_receive() {
        return Poll::Ready(Some(v));
    }
    if inner.closed.load(Ordering::Acquire) {
        if let Ok(v) = inner.chan.try_receive() {
            return Poll::Ready(Some(v));
        }
        return Poll::Ready(None);
    }
    Poll::Pending
}

// ── ChannelFactory impl ───────────────────────────────────────────────

/// [`ChannelFactory`] backed by `embassy-sync::channel::Channel`.
#[derive(Clone, Copy)]
pub struct EmbassySyncChannels;

impl ChannelFactory for EmbassySyncChannels {
    type OneshotSender<T: Send + 'static> = EmbassySyncOneshotSender<T>;
    type OneshotReceiver<T: Send + 'static> = EmbassySyncOneshotReceiver<T>;

    type BoundedSender<T: Send + 'static, const N: usize> = EmbassySyncBoundedSender<T, N>;
    type BoundedReceiver<T: Send + 'static, const N: usize> = EmbassySyncBoundedReceiver<T, N>;

    type UnboundedSender<T: Send + 'static> = EmbassySyncUnboundedSender<T>;
    type UnboundedReceiver<T: Send + 'static> = EmbassySyncUnboundedReceiver<T>;
}

// Blanket `*Pooled` impls. Embassy-sync still heap-allocates per call
// (one `Arc<Inner<...>>` per pair); the goal of these blanket impls
// is API parity with `TokioChannels`, not zero-alloc.
impl<T: Send + 'static> OneshotPooled<EmbassySyncChannels> for T {
    fn oneshot_pair() -> (
        <EmbassySyncChannels as ChannelFactory>::OneshotSender<T>,
        <EmbassySyncChannels as ChannelFactory>::OneshotReceiver<T>,
    ) {
        let inner = Arc::new(OneshotInner::new());
        (
            EmbassySyncOneshotSender {
                inner: inner.clone(),
                sent: false,
            },
            EmbassySyncOneshotReceiver { inner },
        )
    }
}

impl<T: Send + 'static, const N: usize> BoundedPooled<EmbassySyncChannels, N> for T {
    fn bounded_pair() -> (
        <EmbassySyncChannels as ChannelFactory>::BoundedSender<T, N>,
        <EmbassySyncChannels as ChannelFactory>::BoundedReceiver<T, N>,
    ) {
        let inner: Arc<MpscInner<T, N>> = Arc::new(MpscInner::new());
        (
            EmbassySyncBoundedSender {
                inner: inner.clone(),
            },
            EmbassySyncBoundedReceiver { inner },
        )
    }
}

impl<T: Send + 'static> UnboundedPooled<EmbassySyncChannels> for T {
    fn unbounded_pair() -> (
        <EmbassySyncChannels as ChannelFactory>::UnboundedSender<T>,
        <EmbassySyncChannels as ChannelFactory>::UnboundedReceiver<T>,
    ) {
        let inner: Arc<MpscInner<T, UNBOUNDED_CAP>> = Arc::new(MpscInner::new());
        (
            EmbassySyncUnboundedSender {
                inner: inner.clone(),
            },
            EmbassySyncUnboundedReceiver { inner },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::pin::pin;
    use core::task::{Context, Waker};

    fn poll_once<F: Future + Unpin>(fut: &mut F) -> Poll<F::Output> {
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        core::pin::Pin::new(fut).poll(&mut cx)
    }

    #[test]
    fn oneshot_happy_path() {
        let (tx, rx) = <u32 as OneshotPooled<EmbassySyncChannels>>::oneshot_pair();
        tx.send(42).unwrap();
        let mut fut = pin!(rx.recv());
        match fut.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(Ok(42)) => {}
            other => panic!("expected Ready(Ok(42)), got {other:?}"),
        }
    }

    #[test]
    fn oneshot_send_after_receiver_drop_returns_err() {
        let (tx, rx) = <u32 as OneshotPooled<EmbassySyncChannels>>::oneshot_pair();
        drop(rx);
        match tx.send(7) {
            Err(7) => {}
            other => panic!("expected Err(7), got {other:?}"),
        }
    }

    #[test]
    fn oneshot_recv_after_sender_drop_returns_cancelled() {
        let (tx, rx) = <u32 as OneshotPooled<EmbassySyncChannels>>::oneshot_pair();
        drop(tx);
        let mut fut = pin!(rx.recv());
        match fut.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(Err(OneshotCancelled)) => {}
            other => panic!("expected Ready(Err(Cancelled)), got {other:?}"),
        }
    }

    #[test]
    fn unbounded_send_after_receiver_drop_returns_err() {
        let (tx, rx) = <u32 as UnboundedPooled<EmbassySyncChannels>>::unbounded_pair();
        drop(rx);
        match tx.send_now(7) {
            Err(7) => {}
            other => panic!("expected Err(7), got {other:?}"),
        }
    }

    #[test]
    fn bounded_recv_returns_none_when_all_senders_drop() {
        let (tx, mut rx) = <u32 as BoundedPooled<EmbassySyncChannels, 4>>::bounded_pair();
        let tx2 = tx.clone();
        drop(tx);
        // One sender alive — recv must be Pending.
        {
            let mut fut = pin!(rx.recv());
            assert!(matches!(poll_once(&mut fut), Poll::Pending));
        }
        drop(tx2);
        // All senders gone — recv resolves to None.
        let mut fut = pin!(rx.recv());
        match poll_once(&mut fut) {
            Poll::Ready(None) => {}
            other => panic!("expected Ready(None), got {other:?}"),
        }
    }

    #[test]
    fn bounded_send_after_receiver_drop_returns_err_fast_path() {
        let (tx, rx) = <u32 as BoundedPooled<EmbassySyncChannels, 4>>::bounded_pair();
        drop(rx);
        let mut fut = pin!(tx.send(99));
        match poll_once(&mut fut) {
            Poll::Ready(Err(())) => {}
            other => panic!("expected Ready(Err), got {other:?}"),
        }
    }

    #[test]
    fn bounded_send_unblocks_with_err_when_receiver_drops_mid_await() {
        let (tx, rx) = <u32 as BoundedPooled<EmbassySyncChannels, 1>>::bounded_pair();
        // Fill the slot.
        {
            let mut fut = pin!(tx.send(1));
            assert!(matches!(poll_once(&mut fut), Poll::Ready(Ok(()))));
        }
        // Next send must wait.
        let mut send_fut = pin!(tx.send(2));
        assert!(matches!(poll_once(&mut send_fut), Poll::Pending));
        // Drop receiver — sender must observe close on next poll.
        drop(rx);
        match poll_once(&mut send_fut) {
            Poll::Ready(Err(())) => {}
            other => panic!("expected Ready(Err) after receiver drop, got {other:?}"),
        }
    }
}
