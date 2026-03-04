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
//! | [`DiscoveryOnlyPayload`] | Yes | Built-in payload type that supports only SD messages (no heap required) |
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
//! ### Encoding and decoding a SOME/IP-SD message (`no_std`)
//!
//! ```rust
//! use simple_someip::{DiscoveryOnlyPayload, PayloadWireFormat, WireFormat};
//! use simple_someip::protocol::{Message, sd};
//!
//! // Build an SD header with a FindService entry
//! let sd_header: sd::Header<1, 0> = sd::Header::new_find_services(false, &[0x1234]);
//!
//! // Wrap it in a full SOME/IP message
//! let message = Message::<DiscoveryOnlyPayload<1, 0>>::new_sd(0x0001, &sd_header);
//!
//! // Encode to bytes
//! let mut buf = [0u8; 64];
//! let n = message.encode(&mut buf.as_mut_slice()).unwrap();
//!
//! // Decode from bytes (zero-copy view)
//! let view = simple_someip::protocol::MessageView::parse(&buf[..n]).unwrap();
//! assert!(view.is_sd());
//! ```
//!
//! ### Async client — discovering services
//!
#![cfg_attr(feature = "client", doc = "```rust,no_run")]
#![cfg_attr(not(feature = "client"), doc = "```rust,ignore")]
//! use simple_someip::{Client, ClientUpdate, DiscoveryOnlyPayload};
//! use std::net::Ipv4Addr;
//!
//! # #[tokio::main] async fn main() {
//! let mut client = Client::<DiscoveryOnlyPayload>::new(Ipv4Addr::LOCALHOST);
//! client.bind_discovery().await.unwrap();
//!
//! while let Some(update) = client.run().await {
//!     match update {
//!         ClientUpdate::DiscoveryUpdated(msg) => {
//!             println!("SD from {}: {:?}", msg.source, msg.sd_header);
//!         }
//!         ClientUpdate::SenderRebooted(addr) => {
//!             println!("Sender {addr} rebooted");
//!         }
//!         _ => {}
//!     }
//! }
//! # }
//! ```
//!
//! ### Async server — offering a service
//!
#![cfg_attr(feature = "server", doc = "```rust,no_run")]
#![cfg_attr(not(feature = "server"), doc = "```rust,ignore")]
//! use simple_someip::server::{Server, ServerConfig};
//! use std::net::Ipv4Addr;
//!
//! # #[tokio::main] async fn main() {
//! let config = ServerConfig::new(Ipv4Addr::LOCALHOST, 30000, 0x1234, 0x0001);
//! let mut server = Server::<1, 1>::new(config).await.unwrap();
//!
//! // Start periodic SD announcements
//! server.start_announcing().unwrap();
//!
//! // Run the server event loop (handles FindService, subscriptions, etc.)
//! server.run().await.unwrap();
//! # }
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
/// SOME/IP server for offering services and handling incoming requests.
#[cfg(feature = "server")]
pub mod server;
mod traits;
pub use traits::{DiscoveryOnlyPayload, PayloadWireFormat, WireFormat};

#[cfg(feature = "client")]
pub use client::{Client, ClientUpdate, DiscoveryMessage};
#[cfg(feature = "server")]
pub use server::Server;
