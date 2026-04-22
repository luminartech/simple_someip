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
//! use simple_someip::protocol::sd::{self, Entry, RebootFlag, ServiceEntry};
//!
//! // Build an SD header with a FindService entry
//! let entries = [Entry::FindService(ServiceEntry::find(0x1234))];
//! // A fresh process should set RebootFlag::RecentlyRebooted until its
//! // session counter wraps past 0xFFFF for the first time.
//! let sd_header =
//!     sd::Header::new(sd::Flags::new_sd(RebootFlag::RecentlyRebooted), &entries, &[]);
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
//! ### Async client (requires `feature = "client"`)
//!
//! ```rust,no_run
//! # #[cfg(feature = "client")]
//! # fn wrapper() {
//! use simple_someip::{Client, ClientUpdate, RawPayload};
//!
//! #[tokio::main]
//! async fn main() {
//!     // Client::new returns a Clone-able handle and an update stream.
//!     let (client, mut updates) = Client::<RawPayload>::new([192, 168, 1, 100].into());
//!     client.bind_discovery().await.unwrap();
//!
//!     while let Some(update) = updates.recv().await {
//!         match update {
//!             ClientUpdate::DiscoveryUpdated(msg) => { /* SD message received */ }
//!             ClientUpdate::Unicast { message, e2e_status } => { /* unicast reply */ }
//!             ClientUpdate::SenderRebooted(addr) => { /* remote reboot */ }
//!             ClientUpdate::Error(err) => { /* error */ }
//!         }
//!     }
//! }
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

/// Maximum size, in bytes, of UDP payloads for `client` / `server` send
/// paths that serialize into a fixed-size buffer of this size.
///
/// Paths currently capped by this constant:
/// - `client::SocketManager::send` (unicast + SD outbound)
/// - `server::EventPublisher::publish_event`
/// - `server::EventPublisher::publish_raw_event`
///
/// When one of these paths is actually reached and serialization is
/// attempted, messages larger than this cap fail with
/// `client::Error::Capacity("udp_buffer")` or
/// `server::Error::Capacity("udp_buffer")`, depending on the path.
/// Paths that return early before
/// attempting serialization (e.g. `publish_event` when there are no
/// subscribers) are not affected. Other outbound SD paths (announcement
/// builders, `SubscribeAck` / `SubscribeNack`) currently still use
/// heap `Vec` buffers and are not capped by this constant — that is a
/// known gap, planned alongside the bare-metal `no_alloc` refactor.
///
/// Note that this is an application-level UDP payload limit, not an
/// Ethernet-MTU-safe size: a 1500-byte UDP payload exceeds a 1500-byte
/// L2 MTU once IP/UDP headers are added (IPv4 leaves 1472 bytes of UDP
/// payload, IPv6 leaves 1452), so sends at this size may fragment or
/// fail depending on the network stack. Bare-metal ports targeting a
/// smaller link MTU may want to lower this by forking.
pub const UDP_BUFFER_SIZE: usize = 1500;

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
/// Tokio + `socket2` implementation of the [`transport`] traits. Provided
/// as the default `std` backend — available whenever `client` or `server`
/// is enabled.
#[cfg(any(feature = "client", feature = "server"))]
pub mod tokio_transport;
mod traits;
/// Executor-agnostic UDP transport abstraction. `no_std`-compatible.
///
/// Intended to be consumed by the `client` and `server` modules in a
/// future refactor; currently those paths still use `tokio` / `socket2`
/// directly. The trait surface is defined here so bare-metal consumers
/// can implement it today against their own stack and be ready when the
/// higher-level modules are migrated.
pub mod transport;
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
#[cfg(any(feature = "client", feature = "server"))]
pub use tokio_transport::{TokioSocket, TokioTimer, TokioTransport};
pub use transport::{
    IoErrorKind, ReceivedDatagram, SocketOptions, Timer, TransportError, TransportFactory,
    TransportSocket,
};
