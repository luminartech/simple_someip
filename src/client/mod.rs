//! SOME/IP client implementations.
//!
//! Two variants live here, gated by feature flag:
//!
//! - [`tokio_impl`] (feature `client`) — the original tokio-backed
//!   asynchronous client; std-bound, used by desktop tooling like
//!   `iris_someip_client`. Re-exports `Client`, `ClientUpdate`, etc.
//!   at this module's path for backward compatibility.
//! - [`no_std`] (feature `no_std-client`) — a `heapless`-backed
//!   client/server with the same protocol behaviour, suitable for
//!   embedded firmware that drives a polled executor.
//!
//! Both variants share the protocol primitives in
//! [`crate::protocol`], [`crate::e2e`], and the
//! [`crate::runtime`] traits.

#[cfg(feature = "no_std-client")]
pub mod no_std;

#[cfg(feature = "client")]
mod error;
#[cfg(feature = "client")]
mod inner;
#[cfg(feature = "client")]
mod service_registry;
#[cfg(feature = "client")]
mod session;
#[cfg(feature = "client")]
mod tokio_impl;
#[cfg(feature = "client")]
mod wire;

#[cfg(feature = "client")]
pub use error::Error;
#[cfg(feature = "client")]
pub use tokio_impl::*;
