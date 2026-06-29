use simple_someip::buffer_pool::BufferPool;
use simple_someip::transport::{BufferProvider, StaticBufferProvider};

// One pool per test: a shared `static` would let libtest's parallel threads
// race (one test claiming both slots makes the other's claim spuriously fail).
//
// Slot length is 16 — the SOME/IP-header floor enforced by
// `BufferPool::new`'s compile-time `const` assertion. A smaller `LEN` would
// fail to compile.
static POOL_EXHAUST: BufferPool<2, 16> = BufferPool::new();
static POOL_RETURN: BufferPool<2, 16> = BufferPool::new();

#[test]
fn claim_returns_distinct_zeroed_slices_until_exhausted() {
    let mut a = POOL_EXHAUST.claim().expect("slot 0");
    let b = POOL_EXHAUST.claim().expect("slot 1");
    assert_eq!(a.len(), 16);
    assert_eq!(&*b, &[0u8; 16]); // freshly handed-out slot is zeroed
    a[0] = 0xAB; // writable
    assert_eq!(a[0], 0xAB);
    assert!(
        POOL_EXHAUST.claim().is_none(),
        "pool of 2 must refuse a 3rd claim"
    );
}

#[test]
fn dropping_a_lease_returns_its_slot() {
    let a = POOL_RETURN.claim().expect("slot");
    drop(a);
    assert!(
        POOL_RETURN.claim().is_some(),
        "slot must be reusable after the lease drops"
    );
}

static PROV_POOL: BufferPool<2, 16> = BufferPool::new();

#[test]
fn static_provider_claims_through_a_shared_pool() {
    let prov = StaticBufferProvider(&PROV_POOL);
    let _a = prov.claim().expect("first");
    let _b = prov.claim().expect("second");
    assert!(
        prov.claim().is_none(),
        "provider exposes the pool's capacity"
    );
}

/// Concurrent-claim regression (mirrors the channel pools'
/// `*_concurrent_first_claim_does_not_panic`): N threads each call `claim()`
/// on a shared `static` pool of N slots. Asserts (a) all N succeed, (b) each
/// lease gets a distinct slot — verified by writing a unique byte per lease
/// and checking no two leases alias (the "one slot → one lease" invariant
/// under contention), and (c) a further claim returns `None` once all slots
/// are held.
#[test]
fn concurrent_claim_hands_out_distinct_non_aliasing_slots() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Barrier, Mutex};

    const N: usize = 8;
    static POOL: BufferPool<N, 16> = BufferPool::new();

    let success = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(N));
    // Collect each thread's claimed lease so the slots stay held until after
    // we have checked for aliasing and exhaustion (dropping a lease would
    // free its slot and let a later claim succeed).
    let leases = Arc::new(Mutex::new(std::vec::Vec::new()));

    let mut handles = std::vec::Vec::new();
    for tag in 0..N {
        let success = Arc::clone(&success);
        let barrier = Arc::clone(&barrier);
        let leases = Arc::clone(&leases);
        handles.push(std::thread::spawn(move || {
            // Maximize contention: every thread reaches `claim()` together.
            barrier.wait();
            if let Some(mut lease) = POOL.claim() {
                success.fetch_add(1, Ordering::SeqCst);
                // Stamp a unique byte for this thread into its slot. If two
                // leases aliased the same slot, the later write would clobber
                // the earlier one and the per-lease check below would fail.
                let stamp = u8::try_from(tag).unwrap();
                lease[0] = stamp;
                leases.lock().unwrap().push((stamp, lease));
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(
        success.load(Ordering::SeqCst),
        N,
        "all {N} concurrent claims should have succeeded against an {N}-slot pool",
    );

    let leases = leases.lock().unwrap();
    assert_eq!(leases.len(), N, "every claim should have produced a lease");

    // No two leases alias: each lease still reads back its own stamp. Aliasing
    // would have let one thread's write overwrite another's slot.
    for (stamp, lease) in leases.iter() {
        assert_eq!(
            lease[0], *stamp,
            "lease slot aliased — its stamp byte was clobbered by another lease",
        );
    }

    // The pool is fully claimed (all N leases still held), so a further claim
    // must fail.
    assert!(
        POOL.claim().is_none(),
        "an {N}-slot pool with {N} leases outstanding must refuse another claim",
    );
}
