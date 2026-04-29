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
//! | `client` | No | Async client trait surface — service discovery, subscriptions, request/response (feature `client`; add `client-tokio` for `Client::new`) |
//! | `server` | No | Async server trait surface — service offering, event publishing, subscription management (feature `server`; add `server-tokio` for `Server::new`) |
//!
//! ## Feature Flags
//!
//! | Feature | Default | Description |
//! |---------|---------|-------------|
//! | `std` | yes | Enables std-dependent helpers (`RawPayload`, `VecSdHeader`) and the `Arc<Mutex<E2ERegistry>>` / `Arc<RwLock<…>>` default lock-handle impls used by the tokio backends. |
//! | `client` | no | Trait-surface client. Pure `no_std`-clean (does not pull `extern crate alloc`). Caller supplies `Spawner` / `Timer` / `ChannelFactory` / `TransportFactory` / `E2ERegistryHandle` / `InterfaceHandle` impls. |
//! | `client-tokio` | no | Adds the `Client::new` / `TokioSpawner` / `TokioTransport` convenience defaults; implies `client` + std + tokio + socket2. |
//! | `server` | no | Trait-surface server. Pulls `extern crate alloc` (for `Arc<EventPublisher>` / `Arc<F::Socket>`); on `no_std`, downstream consumers must provide a `#[global_allocator]`. |
//! | `server-tokio` | no | Adds the `Server::new` / `TokioTransport` / `TokioTimer` convenience defaults; implies `server` + std + tokio + socket2. |
//! | `bare_metal` | no | Activates embassy-sync, the `static_channels` module (no-alloc `ChannelFactory`), `AtomicInterfaceHandle`, `StaticE2EHandle`, and `StaticSubscriptionHandle`. All five are pure `no_std` (no allocator required). See `examples/bare_metal_client/` and `examples/bare_metal_server/` for runnable bare-metal integration examples. |
//! | `embassy_channels` | no | Heap-backed `EmbassySyncChannels` `ChannelFactory`. Implies `bare_metal` and pulls `extern crate alloc;` into the crate; **on `no_std`, downstream consumers must provide a `#[global_allocator]`**. Useful for tests / early prototypes before sizing static pools. |
//!
//! The default feature set is `["std"]`, which links `std` and enables
//! the `RawPayload` / `VecSdHeader` helpers. For a minimal build with
//! no allocator requirement — the `protocol`, trait, `transport`, and
//! `e2e` modules only — pass `--no-default-features`. The
//! trait-surface canary workspace members (`examples/bare_metal_client`,
//! `examples/bare_metal_server`) depend on the crate with
//! `default-features = false, features = ["bare_metal", "client"]` /
//! `["bare_metal", "server"]` and validate that configuration when built
//! in isolation (`cargo build -p bare_metal_client` /
//! `cargo build -p bare_metal_server`), rather than as part of a workspace-wide
//! build where features may be unified across members.
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
//! ### Async client (requires `feature = "client-tokio"`)
//!
//! ```rust,no_run
//! # #[cfg(feature = "client-tokio")]
//! # fn wrapper() {
//! use simple_someip::{Client, ClientUpdate, RawPayload};
//!
//! #[tokio::main]
//! async fn main() {
//!     // Client::new returns a Clone-able handle, an update stream, and
//!     // the run-loop future. Spawn the future on the tokio runtime;
//!     // the returned future depends on `tokio::select!` / `tokio::time`
//!     // / tokio sockets, so it is not executor-agnostic today.
//!     let (client, mut updates, run) = Client::<RawPayload, _, _, _>::new([192, 168, 1, 100].into());
//!     let _run_task = tokio::spawn(run);
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

// `alloc` is required by:
// - `embassy_channels` — `EmbassySyncChannels` heap-allocates an
//   `Arc<Channel<...>>` per oneshot/bounded/unbounded.
// - `server` — `EventPublisher` and the `Server` struct hold
//   `Arc<EventPublisher<...>>` / `Arc<F::Socket>` for sharing
//   between the run loop and external publishing tasks. A
//   future refactor may switch to `&'static` borrows so the
//   server compiles in pure no_std without an allocator;
//   tracked in `bare_metal_plan_v3.md` Phase 21+ backlog.
//
// The `static_channels` module (under `bare_metal` alone) does
// NOT need alloc — users wanting `client` + `bare_metal` without
// allocator get the no-alloc oneshot/mpsc primitives via the
// macro. Pure `bare_metal` without `client` / `server` /
// `embassy_channels` also stays alloc-free.
#[cfg(any(feature = "embassy_channels", feature = "server"))]
extern crate alloc;

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
///
/// The engine is generic over [`transport::TransportFactory`] +
/// [`transport::Timer`] + [`transport::E2ERegistryHandle`] +
/// [`server::SubscriptionHandle`], so the bare `server` feature exposes the
/// trait-surface server. The `server-tokio` feature additionally provides
/// the tokio convenience constructors (`server::Server::new`,
/// `server::Server::new_with_loopback`, `server::Server::new_passive`)
/// that default the type parameters to
/// `Arc<Mutex<E2ERegistry>>` / `Arc<RwLock<SubscriptionManager>>` /
/// `TokioTransport` / `TokioTimer`.
#[cfg(feature = "server")]
pub mod server;
/// Tokio + `socket2` implementation of the [`transport`] traits. Provided
/// as the default `std` backend — available whenever `client-tokio` or
/// `server-tokio` is enabled.
#[cfg(any(feature = "client-tokio", feature = "server-tokio"))]
pub mod tokio_transport;

/// `embassy-sync`-backed implementation of [`transport::ChannelFactory`].
/// Available whenever the `embassy_channels` feature is enabled. Uses
/// heap allocation (`Arc<Channel<...>>`) — for no-alloc, use
/// [`static_channels`] instead.
#[cfg(feature = "embassy_channels")]
pub mod embassy_channels;
/// Static-pool no-alloc primitives for [`transport::ChannelFactory`].
/// Backs the consumer-declared static `OneshotPool` / `MpscPool`
/// instances that the [`define_static_channels!`] macro
/// generates per-`T` `*Pooled<MyChannels>` impls against.
#[cfg(feature = "bare_metal")]
pub mod static_channels;
mod traits;
/// Executor-agnostic UDP transport abstraction used by the client and
/// server modules. `no_std`-compatible; a default `std + tokio` backend
/// ships in `tokio_transport` (available under the `client-tokio` /
/// `server-tokio` features) — the link is rendered as a code literal
/// because the target module is feature-gated and would break
/// default-feature rustdoc builds.
pub mod transport;
#[cfg(feature = "std")]
pub use raw_payload::{RawPayload, VecSdHeader};
#[cfg(feature = "std")]
pub use traits::OfferedEndpoint;
pub use traits::{PayloadWireFormat, WireFormat};

#[cfg(feature = "client")]
pub use client::{
    Client, ClientDeps, ClientUpdate, ClientUpdates, DiscoveryMessage, PendingResponse,
};
pub use e2e::{E2ECheckStatus, E2EKey, E2EProfile};
#[cfg(feature = "server")]
pub use server::{Server, ServerDeps, ServerHandles, SubscriptionHandle};
#[cfg(any(feature = "client-tokio", feature = "server-tokio"))]
pub use tokio_transport::{TokioChannels, TokioSocket, TokioSpawner, TokioTimer, TokioTransport};
#[cfg(feature = "bare_metal")]
pub use transport::AtomicInterfaceHandle;
pub use transport::{
    ChannelFactory, E2ERegistryHandle, InterfaceHandle, IoErrorKind, LocalSpawner, MpscRecv,
    MpscSend, OneshotCancelled, OneshotRecv, OneshotSend, ReceivedDatagram, SocketOptions, Spawner,
    Timer, TransportError, TransportFactory, TransportSocket, UnboundedRecv, UnboundedSend,
};
#[cfg(feature = "bare_metal")]
pub use transport::{StaticE2EHandle, StaticE2EStorage};
