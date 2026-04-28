//! Phase-16 no-alloc CI gate: prove that the bare-metal handle types and
//! static-pool channels do not invoke the global allocator on the hot path.
//!
//! # Why `harness = false`
//!
//! The standard `#[test]` harness allocates internally (each test run wraps
//! the test in an `Arc` for lifecycle tracking). With a panic-on-alloc
//! `#[global_allocator]` that would fire immediately on test-harness setup,
//! before any of our code runs. `harness = false` removes the harness: this
//! file defines its own `main()` that runs the witness functions directly and
//! exits with a non-zero status (via panic) on any unexpected allocation.
//!
//! # Strategy
//!
//! A [`PanicAllocator`] replaces the global allocator. It is disarmed by
//! default; [`assert_no_alloc`] arms it around a closure, causing any
//! allocation inside the closure to panic — turning a latent regression into
//! a hard CI failure. Because `main()` is single-threaded and all witnessed
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
//! 3. [`define_static_channels!`] oneshot `claim` + `send` do not allocate
//!    after the pool is warmed. The first claim seeds the pool's free-list;
//!    subsequent warm claims are alloc-free.
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
use core::net::Ipv4Addr;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::alloc::{GlobalAlloc, Layout, System};

use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

use simple_someip::e2e::{E2EKey, E2EProfile, E2ERegistry, Profile4Config};
use simple_someip::transport::{AtomicInterfaceHandle, OneshotSend, StaticE2EHandle};
use simple_someip::{
    ChannelFactory, E2ERegistryHandle, InterfaceHandle, StaticE2EStorage, define_static_channels,
};

// ── Panic allocator ───────────────────────────────────────────────────────

static ARMED: AtomicBool = AtomicBool::new(false);

struct PanicAllocator;

unsafe impl GlobalAlloc for PanicAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            panic!(
                "allocation forbidden: {} bytes, align {}",
                layout.size(),
                layout.align()
            );
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
            panic!(
                "allocation forbidden (alloc_zeroed): {} bytes, align {}",
                layout.size(),
                layout.align()
            );
        }
        // SAFETY: forwarding to System.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            panic!(
                "allocation forbidden (realloc): {} → {} bytes",
                layout.size(),
                new_size
            );
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
        Box::leak(Box::new(BlockingMutex::<CriticalSectionRawMutex, RefCell<E2ERegistry>>::new(
            RefCell::new(E2ERegistry::new()),
        )));
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
        assert!(handle.check(E2EKey::new(0xFFFF, 0x0000), b"payload", [0u8; 8]).is_none());
    });
}

fn witness_static_e2e_handle_protect_check() {
    let storage: &'static StaticE2EStorage =
        Box::leak(Box::new(BlockingMutex::<CriticalSectionRawMutex, RefCell<E2ERegistry>>::new(
            RefCell::new(E2ERegistry::new()),
        )));
    let handle = StaticE2EHandle::new(storage);

    handle.register(
        E2EKey::new(0x0001, 0x8001),
        E2EProfile::Profile4(Profile4Config::new(0x1234_5678, 15)),
    );

    let key = E2EKey::new(0x0001, 0x8001);
    let payload = b"hello";
    let mut protected = [0u8; 64];

    assert_no_alloc("StaticE2EHandle::protect + check round-trip", || {
        let len = handle
            .protect(key, payload, [0u8; 8], &mut protected)
            .expect("profile registered")
            .expect("protect succeeded");
        let (status, stripped) =
            handle.check(key, &protected[..len], [0u8; 8]).expect("profile registered");
        assert_eq!(status, simple_someip::E2ECheckStatus::Ok);
        assert_eq!(stripped, payload);
    });
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

// ── Entry point ───────────────────────────────────────────────────────────

fn main() {
    println!("no-alloc witness:");

    witness_atomic_interface_handle();
    witness_static_e2e_handle_reads();
    witness_static_e2e_handle_protect_check();
    witness_static_channels_oneshot();

    println!("all witnesses passed");
}
