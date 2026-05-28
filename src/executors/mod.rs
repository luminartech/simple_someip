//! Optional executor implementations for driving the runtime-agnostic
//! client and server on targets that do not provide an async runtime
//! out of the box.
//!
//! These are pure conveniences — anyone using `simple_someip` can
//! drive its async APIs with any other executor (tokio, embassy,
//! async-std, …) without enabling this module.

#[cfg(feature = "polled-executor")]
pub mod polled;
