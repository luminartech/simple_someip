//! Concrete runtime adapters for the `simple_someip::runtime` traits.
//!
//! Adapters live behind feature flags so the no_std core stays clean: enabling
//! the `tokio-adapter` feature pulls in tokio and exposes [`tokio::TokioUdpSocket`]
//! and [`tokio::TokioClock`].

#[cfg(feature = "tokio-adapter")]
pub mod tokio;
