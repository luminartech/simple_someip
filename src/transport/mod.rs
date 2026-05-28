//! Concrete transport implementations of the `runtime` traits.
//!
//! Where [`crate::adapters`] provides full async-runtime-aware impls
//! (tokio etc.), this module hosts transport patterns tailored to
//! `no_std` embedded use: typically a polled executor backed by host
//! callbacks into a C network stack.

#[cfg(feature = "callback-transport")]
pub mod callback;
