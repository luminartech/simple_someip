//! No-alloc CI gate (server side): proves the `server + bare_metal`
//! configuration's static handle types — specifically
//! [`StaticSubscriptionHandle`] — execute their hot paths without
//! invoking the global allocator.
//!
//! Sibling to [`tests/no_alloc_witness.rs`] (which gates the
//! `client + bare_metal` side). The harness shape is identical: a
//! [`PanicAllocator`] replaces the global allocator and is armed only
//! around the witnessed closures, turning any forbidden allocation
//! into a [`std::process::abort`] (and a hard CI failure).
//!
//! # Why `harness = false`
//!
//! `libtest` allocates during process startup — see the lengthy comment
//! in `no_alloc_witness.rs` for the rationale. The end result: this
//! file defines its own `main()` and reports a single pseudo-test name
//! to cargo-nextest's `--list` probe.
//!
//! # What is witnessed
//!
//! 1. [`StaticSubscriptionHandle::subscribe`] and `unsubscribe` now
//!    return concrete [`core::future::Ready`] futures (the
//!    [`feat/no-alloc-bare-metal`] fork's no-alloc rewrite of the
//!    previously-`Box::pin`'d versions). Constructing the future and
//!    polling it to `Ready` must not allocate.
//! 2. [`StaticSubscriptionHandle::for_each_subscriber`] iterates the
//!    backing storage without allocating — the closure is invoked
//!    per subscriber from inside the critical-section mutex.
//!
//! Together these are the load-bearing additions that let the
//! `server` Cargo feature drop its `_alloc` implication. The
//! upstream `nm` audit on `client,server,bare_metal` is the static
//! complement; this file is the dynamic complement.
//!
//! # What this does not witness
//!
//! - The full `Server::run_with_buffers` loop (requires a no-alloc
//!   `TransportFactory`, `Timer`, and `EventPublisher` — out of scope
//!   for the witness; the `examples/bare_metal_server` workspace
//!   member is the compile-witness for that surface).
//! - Construction-time allocations inside `SubscriptionManager`'s
//!   `heapless::Vec` backing storage. Those are by-design alloc-free
//!   but happen outside the armed window.

#![cfg(all(feature = "server", feature = "bare_metal"))]

use core::cell::{Cell, RefCell};
use core::future::Future;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::{Context, Poll, Waker};
use std::alloc::{GlobalAlloc, Layout, System};
use std::process;

use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

use simple_someip::server::{
    StaticSubscriptionHandle, StaticSubscriptionStorage, SubscriptionHandle, SubscriptionManager,
};

// ── Panic allocator ───────────────────────────────────────────────────────

static ARMED: AtomicBool = AtomicBool::new(false);

struct PanicAllocator;

fn diagnose_and_abort(kind: &str, size: usize, align_or_new: usize) -> ! {
    ARMED.store(false, Ordering::SeqCst);
    eprintln!(
        "no_alloc_server_witness: forbidden allocation ({kind}): {size} bytes / {align_or_new}"
    );
    process::abort();
}

unsafe impl GlobalAlloc for PanicAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ARMED.load(Ordering::Acquire) {
            diagnose_and_abort("alloc", layout.size(), layout.align());
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if ARMED.load(Ordering::Acquire) {
            diagnose_and_abort("alloc_zeroed", layout.size(), layout.align());
        }
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ARMED.load(Ordering::Acquire) {
            diagnose_and_abort("realloc", layout.size(), new_size);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: PanicAllocator = PanicAllocator;

fn assert_no_alloc<T>(label: &str, f: impl FnOnce() -> T) -> T {
    ARMED.store(true, Ordering::SeqCst);
    let result = f();
    ARMED.store(false, Ordering::SeqCst);
    println!("  [pass] {label}");
    result
}

/// Drive a future to completion with a no-op waker on the main thread.
/// All `StaticSubscriptionHandle` futures are synchronous (`Ready`), so
/// a single poll suffices; we panic otherwise to surface any future
/// regression that re-introduces a yield point.
fn poll_once_to_ready<F: Future>(mut fut: Pin<&mut F>) -> F::Output {
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(v) => v,
        Poll::Pending => panic!(
            "StaticSubscriptionHandle future returned Pending; \
             the no-alloc contract requires synchronous completion \
             (no .await inside the critical-section lock)"
        ),
    }
}

// ── Backing storage ───────────────────────────────────────────────────────
//
// `SubscriptionManager::new()` is `const`, so the backing storage can
// live in a plain `static` — no `Box::leak` needed.

static SUBS: StaticSubscriptionStorage =
    BlockingMutex::<CriticalSectionRawMutex, RefCell<SubscriptionManager>>::new(RefCell::new(
        SubscriptionManager::new(),
    ));

// ── Witnesses ─────────────────────────────────────────────────────────────

fn witness_static_subscription_handle() {
    let handle = StaticSubscriptionHandle::new(&SUBS);
    let a1 = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 8001);
    let a2 = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 2), 8002);

    assert_no_alloc("StaticSubscriptionHandle::subscribe", || {
        let mut fut = core::pin::pin!(handle.subscribe(0x5B, 1, 0x01, a1));
        poll_once_to_ready(fut.as_mut()).expect("subscribe must succeed");
        let mut fut = core::pin::pin!(handle.subscribe(0x5B, 1, 0x01, a2));
        poll_once_to_ready(fut.as_mut()).expect("subscribe must succeed");
    });

    assert_no_alloc("StaticSubscriptionHandle::for_each_subscriber", || {
        let count = Cell::new(0usize);
        let mut fut = core::pin::pin!(
            handle.for_each_subscriber(0x5B, 1, 0x01, |_s| count.set(count.get() + 1))
        );
        let visited = poll_once_to_ready(fut.as_mut());
        assert_eq!(visited, 2);
        assert_eq!(count.get(), 2);
    });

    assert_no_alloc("StaticSubscriptionHandle::unsubscribe", || {
        let mut fut = core::pin::pin!(handle.unsubscribe(0x5B, 1, 0x01, a1));
        poll_once_to_ready(fut.as_mut());
    });

    assert_no_alloc(
        "StaticSubscriptionHandle::for_each_subscriber (post-unsub)",
        || {
            let count = Cell::new(0usize);
            let mut fut = core::pin::pin!(
                handle.for_each_subscriber(0x5B, 1, 0x01, |_s| count.set(count.get() + 1))
            );
            let visited = poll_once_to_ready(fut.as_mut());
            assert_eq!(visited, 1);
            assert_eq!(count.get(), 1);
        },
    );
}

// ── Entry point ───────────────────────────────────────────────────────────

fn main() {
    // Mirror `no_alloc_witness.rs`'s nextest discovery hook.
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--list") {
        if !args.iter().any(|a| a == "--ignored") {
            println!("no_alloc_server_witness: test");
        }
        return;
    }

    println!("no-alloc server witness:");

    witness_static_subscription_handle();

    println!("all witnesses passed");
}
