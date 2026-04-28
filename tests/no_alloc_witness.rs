//! No-alloc CI gate: prove that the bare-metal handle types and
//! static-pool channels do not invoke the global allocator on the hot path.
//!
//! # Why `harness = false`
//!
//! `libtest` allocates during process startup — thread-local storage, a
//! worker thread pool for parallel test execution, and per-test bookkeeping
//! (the harness wraps each test in heap-allocated state). With a
//! panic-on-alloc `#[global_allocator]` that would fire before any of our
//! code runs. `harness = false` removes the harness: this file defines its
//! own `main()` that runs the witness functions directly on the main thread
//! and aborts the process on any unexpected allocation.
//!
//! # Strategy
//!
//! A [`PanicAllocator`] replaces the global allocator. It is disarmed by
//! default; [`assert_no_alloc`] arms it around a closure, causing any
//! allocation inside the closure to call `process::abort()` — turning a
//! latent regression into a hard CI failure. Because `main()` is single-threaded and all witnessed
//! operations are synchronous (no yield points), no background allocations
//! can fire while the allocator is armed.
//!
//! # What is witnessed
//!
//! 1. [`AtomicInterfaceHandle`] `get` / `set` are provably alloc-free (thin
//!    pointer to a `static AtomicU32`).
//! 2. [`StaticE2EHandle`] `contains_key` / `protect` / `check` do not
//!    allocate after the registry is configured. Registration itself may
//!    allocate (the backing [`E2ERegistry`] uses a `HashMap`); that is
//!    acceptable as a construction-time cost.
//! 3. [`define_static_channels!`] oneshot first-claim, warm-claim, and
//!    receiver-poll paths are alloc-free. First-claim is exercised on a
//!    pool that has never been touched before (the `u64` variant), which
//!    is the case that runs once at boot on a real bare-metal target.
//!    `recv()` is polled with [`Waker::noop`] so we measure the channel
//!    path without an executor.
//! 4. Both Profile4 and Profile5 protect/check round-trips through
//!    [`StaticE2EHandle`] are alloc-free.
//!
//! # What this does not witness
//!
//! A fully no-alloc `Client` or `Server` run loop additionally requires a
//! no-alloc `Spawner`, no-alloc transport, and a no-tokio executor. That
//! end-to-end harness requires further work. The counting allocator in
//! `tests/static_channels_alloc_witness.rs` covers the channel-storage hot
//! path in a tokio-hosted context; this file extends it to the handle layer
//! with a stricter panic harness.

use core::cell::RefCell;
use core::future::Future;
use core::net::Ipv4Addr;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use core::task::{Context, Waker};
use std::alloc::{GlobalAlloc, Layout, System};
use std::process;

use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

use simple_someip::e2e::{E2EKey, E2EProfile, E2ERegistry, Profile4Config, Profile5Config};
use simple_someip::transport::{AtomicInterfaceHandle, OneshotRecv, OneshotSend, StaticE2EHandle};
use simple_someip::{
    ChannelFactory, E2ERegistryHandle, InterfaceHandle, StaticE2EStorage, define_static_channels,
};

// ── Panic allocator ───────────────────────────────────────────────────────

static ARMED: AtomicBool = AtomicBool::new(false);

struct PanicAllocator;

/// Disarm the allocator, print a diagnostic, then abort.
///
/// We disarm first so the formatter is allowed to allocate while building
/// the diagnostic — otherwise the diagnostic would re-trigger the allocator
/// trap and we'd lose the message. Aborting (rather than panicking) keeps
/// us off the panic-unwind path, whose machinery also allocates.
fn diagnose_and_abort(kind: &str, size: usize, align_or_new: usize) -> ! {
    ARMED.store(false, Ordering::SeqCst);
    eprintln!("no_alloc_witness: forbidden allocation ({kind}): {size} bytes / {align_or_new}",);
    process::abort();
}

unsafe impl GlobalAlloc for PanicAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            diagnose_and_abort("alloc", layout.size(), layout.align());
        }
        // SAFETY: forwarding to System with caller's layout contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarding to System; ptr/layout from System::alloc.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            diagnose_and_abort("alloc_zeroed", layout.size(), layout.align());
        }
        // SAFETY: forwarding to System.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            diagnose_and_abort("realloc", layout.size(), new_size);
        }
        // SAFETY: forwarding to System; invariants upheld by caller.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: PanicAllocator = PanicAllocator;

/// Arm the panic allocator for the duration of `f`, then disarm.
///
/// Any heap allocation inside `f` causes an immediate panic, which exits
/// the process with a non-zero status code — CI failure.
fn assert_no_alloc<T>(label: &str, f: impl FnOnce() -> T) -> T {
    ARMED.store(true, Ordering::SeqCst);
    let result = f();
    ARMED.store(false, Ordering::SeqCst);
    println!("  [pass] {label}");
    result
}

// ── Static channels ───────────────────────────────────────────────────────

define_static_channels! {
    name: WitnessChannels,
    oneshot: [
        (u32, 8),
        // A separate type used exclusively by the first-claim witness so
        // its pool has never been touched before we arm the allocator.
        (u64, 4),
    ],
    bounded: [
        ((u32, 4), 2),
    ],
    unbounded: [
        (u32, 2),
    ],
}

// ── Backing statics ───────────────────────────────────────────────────────

static IFACE_ADDR: AtomicU32 = AtomicU32::new(0);

// ── Witness functions ─────────────────────────────────────────────────────

fn witness_atomic_interface_handle() {
    let handle = AtomicInterfaceHandle::new(&IFACE_ADDR);
    // Initialize outside the armed window.
    handle.set(Ipv4Addr::LOCALHOST);

    assert_no_alloc("AtomicInterfaceHandle::set / ::get", || {
        handle.set(Ipv4Addr::new(192, 168, 1, 1));
        assert_eq!(handle.get(), Ipv4Addr::new(192, 168, 1, 1));
        handle.set(Ipv4Addr::LOCALHOST);
        assert_eq!(handle.get(), Ipv4Addr::LOCALHOST);
    });
}

fn witness_static_e2e_handle_reads() {
    // Box::leak allocates — that is an accepted construction-time cost.
    let storage: &'static StaticE2EStorage =
        Box::leak(Box::new(BlockingMutex::<
            CriticalSectionRawMutex,
            RefCell<E2ERegistry>,
        >::new(RefCell::new(E2ERegistry::new()))));
    let handle = StaticE2EHandle::new(storage);

    // register() allocates into the HashMap — also construction-time.
    handle.register(
        E2EKey::new(0x1234, 0x0001),
        E2EProfile::Profile4(Profile4Config::new(0xDEAD_BEEF, 15)),
    );

    // Hot-path reads must be alloc-free.
    assert_no_alloc("StaticE2EHandle::contains_key (hit)", || {
        assert!(handle.contains_key(&E2EKey::new(0x1234, 0x0001)));
    });

    assert_no_alloc("StaticE2EHandle::contains_key (miss)", || {
        assert!(!handle.contains_key(&E2EKey::new(0xFFFF, 0x0000)));
    });

    assert_no_alloc("StaticE2EHandle::check (absent key → None)", || {
        assert!(
            handle
                .check(E2EKey::new(0xFFFF, 0x0000), b"payload", [0u8; 8])
                .is_none()
        );
    });
}

fn witness_static_e2e_handle_protect_check() {
    let storage: &'static StaticE2EStorage =
        Box::leak(Box::new(BlockingMutex::<
            CriticalSectionRawMutex,
            RefCell<E2ERegistry>,
        >::new(RefCell::new(E2ERegistry::new()))));
    let handle = StaticE2EHandle::new(storage);

    handle.register(
        E2EKey::new(0x0001, 0x8001),
        E2EProfile::Profile4(Profile4Config::new(0x1234_5678, 15)),
    );
    // Register a second profile (Profile5) so the protect/check witness
    // covers both profile families' hot paths, not just Profile4.
    handle.register(
        E2EKey::new(0x0002, 0x8002),
        // data_length must equal payload length (5 = b"hello".len())
        // — a mismatch routes through `tracing::warn!`, which is fine in
        // production but adds noise to a no-alloc witness.
        E2EProfile::Profile5(Profile5Config::new(0xABCD, 5, 15)),
    );

    let key = E2EKey::new(0x0001, 0x8001);
    let payload = b"hello";
    let mut protected = [0u8; 64];

    assert_no_alloc(
        "StaticE2EHandle::protect + check round-trip (Profile4)",
        || {
            let len = handle
                .protect(key, payload, [0u8; 8], &mut protected)
                .expect("profile registered")
                .expect("protect succeeded");
            let (status, stripped) = handle
                .check(key, &protected[..len], [0u8; 8])
                .expect("profile registered");
            assert_eq!(status, simple_someip::E2ECheckStatus::Ok);
            assert_eq!(stripped, payload);
        },
    );

    let key5 = E2EKey::new(0x0002, 0x8002);
    let mut protected5 = [0u8; 64];
    assert_no_alloc(
        "StaticE2EHandle::protect + check round-trip (Profile5)",
        || {
            let len = handle
                .protect(key5, payload, [0u8; 8], &mut protected5)
                .expect("profile registered")
                .expect("protect succeeded");
            let (status, stripped) = handle
                .check(key5, &protected5[..len], [0u8; 8])
                .expect("profile registered");
            assert_eq!(status, simple_someip::E2ECheckStatus::Ok);
            assert_eq!(stripped, payload);
        },
    );
}

fn witness_static_channels_oneshot() {
    // Warm the pool: first claim/release seeds the free-list.
    {
        let (tx, _rx) = WitnessChannels::oneshot::<u32>();
        tx.send(42u32).ok();
    }

    // Second claim must not allocate.
    assert_no_alloc("WitnessChannels::oneshot warm claim + send", || {
        let (tx, _rx) = WitnessChannels::oneshot::<u32>();
        tx.send(99u32).ok();
    });
}

/// First-claim witness: a freshly declared static pool (the `u64` variant
/// in [`WitnessChannels`], untouched until this point) must seed its
/// free-list and hand out the first slot without allocating. This is the
/// case that runs once at boot on a real bare-metal target.
fn witness_static_channels_first_claim() {
    assert_no_alloc("WitnessChannels::oneshot::<u64> FIRST claim + send", || {
        let (tx, _rx) = WitnessChannels::oneshot::<u64>();
        tx.send(7u64).ok();
    });
}

/// Receiver hot-path witness: polling the recv future once on a slot that
/// already has a value must not allocate. Uses [`Waker::noop`] so we don't
/// drag in an executor.
fn witness_static_channels_oneshot_recv() {
    // Warm the pool first so this witness measures only the recv path.
    {
        let (tx, _rx) = WitnessChannels::oneshot::<u32>();
        tx.send(1u32).ok();
    }

    assert_no_alloc(
        "WitnessChannels::oneshot recv (value already pending)",
        || {
            let (tx, rx) = WitnessChannels::oneshot::<u32>();
            tx.send(123u32).ok();
            let mut fut = rx.recv();
            // SAFETY: `fut` is stack-pinned and dropped before this scope ends;
            // no reference escapes.
            let pinned = unsafe { Pin::new_unchecked(&mut fut) };
            let waker = Waker::noop();
            let mut cx = Context::from_waker(waker);
            match pinned.poll(&mut cx) {
                core::task::Poll::Ready(Ok(v)) => assert_eq!(v, 123),
                other => panic!("expected Ready(Ok(123)), got {other:?}"),
            }
        },
    );
}

// ── Entry point ───────────────────────────────────────────────────────────

fn main() {
    // cargo-nextest runs `--list --format terse` for test discovery. A
    // `harness = false` binary must print each test name followed by
    // `: test` or `: benchmark`. We expose a single pseudo-test named
    // `no_alloc_witness` so nextest can schedule us.
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--list") {
        // nextest calls --list twice: once for normal tests and once with
        // --ignored. Print nothing for the --ignored pass so nextest does
        // not classify this test as ignored and skip it by default.
        if !args.iter().any(|a| a == "--ignored") {
            println!("no_alloc_witness: test");
        }
        return;
    }

    println!("no-alloc witness:");

    witness_atomic_interface_handle();
    witness_static_e2e_handle_reads();
    witness_static_e2e_handle_protect_check();
    witness_static_channels_first_claim();
    witness_static_channels_oneshot();
    witness_static_channels_oneshot_recv();

    println!("all witnesses passed");
}
