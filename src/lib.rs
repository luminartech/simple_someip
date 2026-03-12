//! # Simple SOME/IP
//!
//! [![CI](https://img.shields.io/github/actions/workflow/status/luminartech/simple_someip/ci.yml?style=for-the-badge&label=CI)](https://github.com/luminartech/simple_someip/actions/workflows/ci.yml)
//! [![Coverage](https://img.shields.io/codecov/c/github/luminartech/simple_someip?style=for-the-badge)](https://app.codecov.io/gh/luminartech/simple_someip)
//! [![Crates.io](https://img.shields.io/crates/v/simple-someip?style=for-the-badge)](https://crates.io/crates/simple-someip)
//!
//! A Rust implementation of the [SOME/IP](https://github.com/some-ip-com/open-someip-spec)
//! automotive communication protocol — remote procedure calls, event notifications, service
//! discovery, and wire-format serialization.
//!
//! The core protocol layer (`protocol`, `e2e`, and trait modules) is `no_std`-compatible with
//! zero heap allocation, making it suitable for embedded targets. Optional `client` and `server`
//! modules provide async tokio-based networking for `std` environments.
//!
//! ## Modules
//!
//! | Module | `no_std` | Description |
//! |--------|----------|-------------|
//! | [`protocol`] | Yes | Wire format: headers, messages, message types, return codes, and service discovery (SD) entries/options |
//! | [`e2e`] | Yes | End-to-End protection — Profile 4 (CRC-32) and Profile 5 (CRC-16) |
//! | [`WireFormat`] / [`PayloadWireFormat`] | Yes | Traits for serializing messages and defining custom payload types |
//! | [`client`] | No | Async tokio client — service discovery, subscriptions, and request/response (feature `client`) |
//! | [`server`] | No | Async tokio server — service offering, event publishing, and subscription management (feature `server`) |
//!
//! ## Feature Flags
//!
//! | Feature | Default | Description |
//! |---------|---------|-------------|
//! | `client` | no | Async tokio client; implies `std` + tokio + socket2 |
//! | `server` | no | Async tokio server; implies `std` + tokio + socket2 |
//! | `std` | no | Enables std-dependent helpers |
//!
//! By default only the `protocol`, trait, and `e2e` modules are compiled, and the crate
//! builds in `no_std` mode with no allocator requirement.
//!
//! ## Examples
//!
//! ### Encoding a SOME/IP-SD header (`no_std`)
//!
//! ```rust
//! use simple_someip::WireFormat;
//! use simple_someip::protocol::sd::{self, Entry, ServiceEntry};
//!
//! // Build an SD header with a FindService entry
//! let entries = [Entry::FindService(ServiceEntry::find(0x1234))];
//! let sd_header = sd::Header::new(sd::Flags::new_sd(false), &entries, &[]);
//!
//! // Encode to bytes
//! let mut buf = [0u8; 64];
//! let n = sd_header.encode(&mut buf.as_mut_slice()).unwrap();
//!
//! // Decode from bytes (zero-copy view)
//! let view = sd::SdHeaderView::parse(&buf[..n]).unwrap();
//! assert_eq!(view.entry_count(), 1);
//! ```
//!
//! ## References
//!
//! - [Open SOME/IP Specification](https://github.com/some-ip-com/open-someip-spec)

#![no_std]
#![warn(clippy::pedantic)]

#[cfg(feature = "std")]
extern crate std;

/// SOME/IP client for discovering services and exchanging messages.
#[cfg(feature = "client")]
pub mod client;
/// End-to-end (E2E) protection utilities for SOME/IP payloads.
pub mod e2e;
/// SOME/IP protocol primitives: headers, messages, return codes, and service discovery.
pub mod protocol;
/// A general-purpose, heap-allocated [`PayloadWireFormat`] implementation.
#[cfg(feature = "std")]
mod raw_payload;
/// SOME/IP server for offering services and handling incoming requests.
#[cfg(feature = "server")]
pub mod server;
mod traits;
#[cfg(feature = "std")]
pub use raw_payload::{RawPayload, VecSdHeader};
#[cfg(feature = "std")]
pub use traits::OfferedEndpoint;
pub use traits::{PayloadWireFormat, WireFormat};

#[cfg(feature = "client")]
pub use client::{Client, ClientUpdate, ClientUpdates, DiscoveryMessage, PendingResponse};
pub use e2e::{E2ECheckStatus, E2EKey, E2EProfile};
#[cfg(feature = "server")]
pub use server::Server;
