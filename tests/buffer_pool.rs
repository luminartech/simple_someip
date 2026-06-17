use simple_someip::static_channels::BufferPool;

static POOL: BufferPool<2, 4> = BufferPool::new();

#[test]
fn claim_returns_distinct_zeroed_slices_until_exhausted() {
    let mut a = POOL.claim().expect("slot 0");
    let b = POOL.claim().expect("slot 1");
    assert_eq!(a.len(), 4);
    assert_eq!(&*b, &[0u8; 4]);            // freshly handed-out slot is zeroed
    a[0] = 0xAB;                            // writable
    assert_eq!(a[0], 0xAB);
    assert!(POOL.claim().is_none(), "pool of 2 must refuse a 3rd claim");
}

#[test]
fn dropping_a_lease_returns_its_slot() {
    let a = POOL.claim().expect("slot");
    drop(a);
    assert!(POOL.claim().is_some(), "slot must be reusable after the lease drops");
}
