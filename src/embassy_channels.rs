//! [`ChannelFactory`] backed by `embassy-sync::channel::Channel`. Active
//! when the `bare_metal` feature is enabled, independent of the tokio
//! backend.
//!
//! # Heap allocation per call
//!
//! Both sender and receiver hold an `Arc<Channel<M, T, N>>`, and every
//! call to [`EmbassySyncChannels::oneshot`], [`bounded`], or
//! [`unbounded`] heap-allocates a fresh `Arc<Channel<...>>`. The
//! `Client` run-loop calls these per request-response pair — most
//! notably, every method on `Client` that awaits a server response
//! constructs a oneshot via this factory, so each such method
//! triggers one `Arc` allocation.
//!
//! This violates the strategic bare-metal goal "zero heap after
//! `Client::new` returns." The fix is a static-pool `ChannelFactory`
//! impl (planned as `StaticChannels<const POOL_SIZE: usize>`) that
//! hands out indices into a pre-allocated `static` array of
//! `Channel`s; that work is its own phase because it may require a
//! `ChannelFactory` trait-shape adjustment to permit `&'static Sender`
//! / `&'static Receiver` ownership. Until that lands, this impl is
//! useful for two cases:
//!
//! 1. Bringing up a bare-metal port end-to-end on `std + alloc`
//!    targets, validating the trait surface before the no-alloc
//!    push.
//! 2. Demonstrating the `ChannelFactory` integration shape for
//!    consumers writing their own no-alloc impl.
//!
//! [`bounded`]: ChannelFactory::bounded
//! [`unbounded`]: ChannelFactory::unbounded

use alloc::sync::Arc;
use core::future::Future;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;

use crate::transport::{
    ChannelFactory, MpscRecv, MpscSend, OneshotCancelled, OneshotRecv, OneshotSend, UnboundedRecv,
    UnboundedSend,
};

// ── Oneshot (capacity-1 Channel) ──────────────────────────────────────

pub struct EmbassySyncOneshotSender<T: Send + 'static>(
    Arc<Channel<CriticalSectionRawMutex, T, 1>>,
);

pub struct EmbassySyncOneshotReceiver<T: Send + 'static>(
    Arc<Channel<CriticalSectionRawMutex, T, 1>>,
);

impl<T: Send + 'static> OneshotSend<T> for EmbassySyncOneshotSender<T> {
    fn send(self, value: T) -> Result<(), T> {
        self.0.try_send(value).map_err(|e| match e {
            embassy_sync::channel::TrySendError::Full(v) => v,
        })
    }
}

impl<T: Send + 'static> OneshotRecv<T> for EmbassySyncOneshotReceiver<T> {
    fn recv(self) -> impl Future<Output = Result<T, OneshotCancelled>> + Send {
        let chan = self.0;
        async move { Ok(chan.receive().await) }
    }
}

// ── Bounded MPSC ──────────────────────────────────────────────────────

pub struct EmbassySyncBoundedSender<T: Send + 'static, const N: usize>(
    Arc<Channel<CriticalSectionRawMutex, T, N>>,
);

pub struct EmbassySyncBoundedReceiver<T: Send + 'static, const N: usize>(
    Arc<Channel<CriticalSectionRawMutex, T, N>>,
);

impl<T: Send + 'static, const N: usize> Clone for EmbassySyncBoundedSender<T, N> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T: Send + 'static, const N: usize> MpscSend<T> for EmbassySyncBoundedSender<T, N> {
    fn send(&self, value: T) -> impl Future<Output = Result<(), ()>> + Send + '_ {
        let chan = self.0.clone();
        async move {
            chan.send(value).await;
            Ok(())
        }
    }
}

impl<T: Send + 'static, const N: usize> MpscRecv<T> for EmbassySyncBoundedReceiver<T, N> {
    fn recv(&mut self) -> impl Future<Output = Option<T>> + Send + '_ {
        let chan = self.0.clone();
        async move { Some(chan.receive().await) }
    }

    fn poll_recv(
        &mut self,
        cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Option<T>> {
        use core::pin::Pin;
        // Try non-blocking receive first.
        if let Ok(val) = self.0.try_receive() {
            return core::task::Poll::Ready(Some(val));
        }
        // Channel is empty. Poll a ReceiveFuture to register the waker.
        // SAFETY: `fut` is created, pinned (stack-only), polled once, then
        // dropped immediately. No references to `fut` escape this scope.
        let mut fut = self.0.receive();
        // SAFETY: ReceiveFuture borrows self.0 (via Arc) — not self — and
        // is not moved after this pin. The Arc ensures the channel outlives
        // the future.
        let pinned = unsafe { Pin::new_unchecked(&mut fut) };
        match pinned.poll(cx) {
            core::task::Poll::Ready(val) => core::task::Poll::Ready(Some(val)),
            core::task::Poll::Pending => core::task::Poll::Pending,
        }
    }
}

// ── Unbounded (large-capacity) MPSC ──────────────────────────────────

// Embassy-sync has no truly unbounded channel; we use a large capacity
// (128) as a practical substitute for the client's update channel.
const UNBOUNDED_CAP: usize = 128;

pub struct EmbassySyncUnboundedSender<T: Send + 'static>(
    Arc<Channel<CriticalSectionRawMutex, T, UNBOUNDED_CAP>>,
);

pub struct EmbassySyncUnboundedReceiver<T: Send + 'static>(
    Arc<Channel<CriticalSectionRawMutex, T, UNBOUNDED_CAP>>,
);

impl<T: Send + 'static> Clone for EmbassySyncUnboundedSender<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T: Send + 'static> UnboundedSend<T> for EmbassySyncUnboundedSender<T> {
    fn send_now(&self, value: T) -> Result<(), T> {
        self.0.try_send(value).map_err(|e| match e {
            embassy_sync::channel::TrySendError::Full(v) => v,
        })
    }
}

impl<T: Send + 'static> UnboundedRecv<T> for EmbassySyncUnboundedReceiver<T> {
    fn recv(&mut self) -> impl Future<Output = Option<T>> + Send + '_ {
        let chan = self.0.clone();
        async move { Some(chan.receive().await) }
    }
}

// ── ChannelFactory impl ───────────────────────────────────────────────

/// [`ChannelFactory`] backed by `embassy-sync::channel::Channel`.
#[derive(Clone, Copy)]
pub struct EmbassySyncChannels;

impl ChannelFactory for EmbassySyncChannels {
    type OneshotSender<T: Send + 'static> = EmbassySyncOneshotSender<T>;
    type OneshotReceiver<T: Send + 'static> = EmbassySyncOneshotReceiver<T>;
    fn oneshot<T: Send + 'static>() -> (Self::OneshotSender<T>, Self::OneshotReceiver<T>) {
        let chan = Arc::new(Channel::new());
        (
            EmbassySyncOneshotSender(chan.clone()),
            EmbassySyncOneshotReceiver(chan),
        )
    }

    // Phase 13.6: the const-N quirk is fixed. The `N` from the trait
    // call site now propagates into the embassy `Channel<_, T, N>`
    // storage, so callers asking for capacity 16 actually get 16, and
    // callers asking for 4 actually get 4. (Previously this impl
    // hardcoded 16 regardless of the requested N.)
    type BoundedSender<T: Send + 'static, const N: usize> = EmbassySyncBoundedSender<T, N>;
    type BoundedReceiver<T: Send + 'static, const N: usize> = EmbassySyncBoundedReceiver<T, N>;
    fn bounded<T: Send + 'static, const N: usize>(
    ) -> (Self::BoundedSender<T, N>, Self::BoundedReceiver<T, N>) {
        let chan: Arc<Channel<CriticalSectionRawMutex, T, N>> = Arc::new(Channel::new());
        (
            EmbassySyncBoundedSender(chan.clone()),
            EmbassySyncBoundedReceiver(chan),
        )
    }

    type UnboundedSender<T: Send + 'static> = EmbassySyncUnboundedSender<T>;
    type UnboundedReceiver<T: Send + 'static> = EmbassySyncUnboundedReceiver<T>;
    fn unbounded<T: Send + 'static>(
    ) -> (Self::UnboundedSender<T>, Self::UnboundedReceiver<T>) {
        let chan = Arc::new(Channel::new());
        (
            EmbassySyncUnboundedSender(chan.clone()),
            EmbassySyncUnboundedReceiver(chan),
        )
    }
}
