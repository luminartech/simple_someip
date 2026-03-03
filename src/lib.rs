//! # Simple SOME/IP (Scalable service-Oriented Middleware over IP)
//!
//! SOME/IP is an automotive/embedded communication protocol which supports remote procedure calls,
//! event notifications and the underlying serialization/wire format.
//!
//! This library attempts to expose an ergonomic API for communicating over SOME/IP.
//! This includes encoding/decoding messages, handling the underlying transport,
//! and providing traits to kickstart the development of client/server applications.
//!
//! This library is based on the [Open SOME/IP Specification](https://github.com/some-ip-com/open-someip-spec).
//!
//! ## Design
//!
//! ## References
//!
//! - [Open SOME/IP Specification](https://github.com/some-ip-com/open-someip-spec)

#![no_std]
#![warn(clippy::pedantic)]
// TODO: Add `# Errors` and `# Panics` doc sections in a follow-up PR.
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]

#[cfg(feature = "std")]
extern crate std;

#[cfg(feature = "client")]
mod client;
pub mod e2e;
mod error;
pub mod protocol;
#[cfg(feature = "server")]
pub mod server;
mod traits;
pub use traits::{DiscoveryOnlyPayload, PayloadWireFormat, WireFormat};

#[cfg(feature = "client")]
pub use client::{Client, ClientUpdate};
pub use error::Error;
#[cfg(feature = "server")]
pub use server::Server;
