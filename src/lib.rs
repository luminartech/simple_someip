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

/// SOME/IP client for discovering services and exchanging messages.
#[cfg(feature = "client")]
pub mod client;
/// End-to-end (E2E) protection utilities for SOME/IP payloads.
pub mod e2e;
/// SOME/IP protocol primitives: headers, messages, return codes, and service discovery.
pub mod protocol;
/// SOME/IP server for offering services and handling incoming requests.
#[cfg(feature = "server")]
pub mod server;
mod traits;
pub use traits::{DiscoveryOnlyPayload, PayloadWireFormat, WireFormat};

#[cfg(feature = "client")]
pub use client::{Client, ClientUpdate, DiscoveryMessage};
#[cfg(feature = "server")]
pub use server::Server;
