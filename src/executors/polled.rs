//! Sync-polled mini-executor for `no_std`, `no_alloc` embedded firmware.
//!
//! Drives a single externally-stored pinned future via repeated polling.
//! Wakeups are assumed to be **externally driven** — typically a periodic
//! tick or interrupt that re-enters the firmware to call [`poll_future`].
//! The `Waker` handed to `Future::poll` is therefore a no-op: any
//! internal call to `wake()` would be silently dropped, but that is
//! acceptable here because every `Pending` state retries on the next
//! external tick anyway.
//!
//! ## Pairing with TAIT for static storage
//!
//! Async functions in stable Rust return opaque `impl Future` types
//! that cannot be named, which makes static storage of a long-running
//! run-future awkward in `no_alloc` builds. The expected pattern is
//! to use `#![feature(type_alias_impl_trait)]` in the consumer crate
//! to name the future type, then store it in a `static` slot. Once
//! pinned, the `Pin<&mut F>` is fed to [`poll_future`] from the host
//! tick.
//!
//! ```ignore
//! #![feature(type_alias_impl_trait)]
//!
//! use simple_someip::executors::polled;
//!
//! type RunFut = impl core::future::Future<Output = ()>;
//!
//! static FUT: SyncCell<MaybeUninit<RunFut>> = ...;
//! static FUT_INIT: AtomicBool = AtomicBool::new(false);
//!
//! pub extern "C" fn someip_init() {
//!     // store fresh future in FUT, mark FUT_INIT
//! }
//!
//! pub extern "C" fn someip_poll(_elapsed_ms: u32) {
//!     if FUT_INIT.load(Ordering::Acquire) {
//!         let pinned = unsafe { core::pin::Pin::new_unchecked(&mut *FUT.get()) };
//!         polled::poll_future(pinned);
//!     }
//! }
//! ```

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

static NOOP_VTABLE: RawWakerVTable = RawWakerVTable::new(
    /* clone       */ noop_clone,
    /* wake        */ noop_wake,
    /* wake_by_ref */ noop_wake,
    /* drop        */ noop_drop,
);

const fn noop_clone(_: *const ()) -> RawWaker {
    RawWaker::new(core::ptr::null(), &NOOP_VTABLE)
}
const fn noop_wake(_: *const ()) {}
const fn noop_drop(_: *const ()) {}

/// Build a `Waker` whose `wake` / `wake_by_ref` / `drop` are all no-ops.
///
/// Use this when wakeups are driven externally (a host tick or
/// interrupt) and the polled future therefore does not need to
/// arrange its own wake-ups.
#[must_use]
pub fn noop_waker() -> Waker {
    // SAFETY: every vtable entry is a no-op that does not dereference
    // the data pointer, so the null data pointer is sound.
    unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &NOOP_VTABLE)) }
}

/// Poll a pinned future once with the no-op waker.
///
/// Returns the future's output if it completes, or `None` if it
/// returned `Pending`. The caller is responsible for keeping the
/// future pinned across calls (typically via a static `UnsafeCell` or
/// `SyncCell` wrapping `MaybeUninit<F>` and pin-projecting through it).
///
/// This primitive only works correctly for futures that are driven
/// by **external** events — e.g. fresh socket bytes / clock advances
/// / queue insertions arriving between calls. Futures that rely on
/// internal `Waker::wake` calls will not be re-polled by this
/// executor and may stall.
pub fn poll_future<F: Future>(fut: Pin<&mut F>) -> Option<F::Output> {
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    match fut.poll(&mut cx) {
        Poll::Ready(v) => Some(v),
        Poll::Pending => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::pin::pin;

    #[test]
    fn ready_future_returns_output() {
        let result = poll_future(pin!(async { 42 }));
        assert_eq!(result, Some(42));
    }

    #[test]
    fn pending_future_returns_none() {
        let result = poll_future::<core::future::Pending<i32>>(pin!(core::future::pending()));
        assert_eq!(result, None);
    }

    #[test]
    fn future_can_be_polled_multiple_times_until_ready() {
        // A future that returns Pending once, then Ready on the second poll.
        // Mirrors how a real I/O-bound future behaves once data arrives.
        struct ReadyOnSecondPoll {
            polled: bool,
        }
        impl Future for ReadyOnSecondPoll {
            type Output = u32;
            fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<u32> {
                if self.polled {
                    Poll::Ready(7)
                } else {
                    self.polled = true;
                    Poll::Pending
                }
            }
        }

        let mut fut = ReadyOnSecondPoll { polled: false };
        // SAFETY: `fut` is owned locally and not moved while pinned/polled.
        let mut fut = unsafe { Pin::new_unchecked(&mut fut) };
        assert_eq!(poll_future(fut.as_mut()), None);
        assert_eq!(poll_future(fut.as_mut()), Some(7));
    }
}
