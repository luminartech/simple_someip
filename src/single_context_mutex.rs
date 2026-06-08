//! A `Sync`, no-op [`RawMutex`] for single-execution-context builds.
//!
//! The polled engine ([`crate::polled`]) advances the whole protocol
//! from one periodic tick on one core, with nothing preempting it to
//! touch the shared registry / subscription state. In that model a
//! critical section buys nothing: there is no second context to lock
//! out. [`SingleContextRawMutex`] therefore implements [`RawMutex`]
//! with a no-op `lock`, which removes every `critical_section::with`
//! call (and the `_critical_section_1_0_*` ABI symbols) from the polled
//! link — bare-metal consumers no longer need to supply a
//! critical-section impl just to satisfy embassy-sync.

use embassy_sync::blocking_mutex::raw::RawMutex;

/// No-op [`RawMutex`]: runs the guarded closure directly, providing no
/// mutual exclusion.
///
/// A fieldless unit type, so it is trivially `Send + Sync`; the mutual-
/// exclusion contract lives in the `unsafe impl RawMutex` below.
pub struct SingleContextRawMutex;

// SAFETY: `RawMutex` requires `lock` to grant exclusive access for the
// duration of the closure. This impl grants none — it is sound ONLY
// when every access happens from a single execution context with no
// preemption or cross-core sharing of the guarded data (the polled
// `bare_metal_poll` contract). It must never back state reachable from
// an ISR or a second core; under those conditions the no-op `lock`
// permits a data race. The `StaticHandleRawMutex` alias gates selection
// on `bare_metal_poll` so this type is only chosen for that contract.
unsafe impl RawMutex for SingleContextRawMutex {
    const INIT: Self = SingleContextRawMutex;

    #[inline(always)]
    fn lock<R>(&self, f: impl FnOnce() -> R) -> R {
        f()
    }
}

/// `RawMutex` backing the `&'static` no-alloc static handles
/// ([`StaticE2EHandle`] / [`StaticSubscriptionHandle`]).
///
/// Polled builds (`bare_metal_poll`) run single-context, so the handles
/// use the no-op [`SingleContextRawMutex`] — no critical section, no
/// `critical-section` impl to provide. Other bare-metal builds (async
/// `client` / `server` without polling) keep the real
/// `CriticalSectionRawMutex`, which is sound across the executor's
/// wakeups and any ISR-driven access.
///
/// [`StaticE2EHandle`]: crate::transport::bare_metal_e2e_impl::StaticE2EHandle
/// [`StaticSubscriptionHandle`]: crate::server::subscription_manager::bare_metal_subscription_impl::StaticSubscriptionHandle
#[cfg(feature = "bare_metal_poll")]
pub type StaticHandleRawMutex = SingleContextRawMutex;

/// See the `bare_metal_poll` variant above.
#[cfg(not(feature = "bare_metal_poll"))]
pub type StaticHandleRawMutex = embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
