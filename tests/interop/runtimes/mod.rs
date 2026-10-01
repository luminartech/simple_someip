//! One `SomeipUnderTest` adapter per runtime under test.

#[cfg(feature = "bare-metal-runtime")]
pub mod bare_metal_rt;
#[cfg(all(feature = "client-tokio", feature = "server-tokio"))]
pub mod std_rt;
