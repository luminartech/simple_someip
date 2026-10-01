//! One `SomeipUnderTest` adapter per runtime under test.

#[cfg(all(feature = "client-tokio", feature = "server-tokio"))]
pub mod std_rt;
