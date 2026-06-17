use simple_someip::static_channels::BufferPool;

// One pool per test: a shared `static` would let libtest's parallel threads
// race (one test claiming both slots makes the other's claim spuriously fail).
static POOL_EXHAUST: BufferPool<2, 4> = BufferPool::new();
static POOL_RETURN: BufferPool<2, 4> = BufferPool::new();

#[test]
fn claim_returns_distinct_zeroed_slices_until_exhausted() {
    let mut a = POOL_EXHAUST.claim().expect("slot 0");
    let b = POOL_EXHAUST.claim().expect("slot 1");
    assert_eq!(a.len(), 4);
    assert_eq!(&*b, &[0u8; 4]);            // freshly handed-out slot is zeroed
    a[0] = 0xAB;                            // writable
    assert_eq!(a[0], 0xAB);
    assert!(POOL_EXHAUST.claim().is_none(), "pool of 2 must refuse a 3rd claim");
}

#[test]
fn dropping_a_lease_returns_its_slot() {
    let a = POOL_RETURN.claim().expect("slot");
    drop(a);
    assert!(POOL_RETURN.claim().is_some(), "slot must be reusable after the lease drops");
}
