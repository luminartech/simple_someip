//! embassy-net `TransportFactory` / `TransportSocket` adapter for
//! [`simple-someip`].
//!
//! This crate is the **reference `no_std` backend** for `simple-someip`'s
//! transport-trait surface. It wraps [`embassy_net::udp::UdpSocket`]
//! behind [`simple_someip::transport::TransportSocket`] and provides a
//! [`simple_someip::transport::TransportFactory`] that hands out sockets
//! from a caller-declared `&'static` storage pool.
//!
//! # Why this crate exists
//!
//! Phase 18 of the bare-metal effort closed the literal compile gate:
//! `simple-someip` + `client,server,bare_metal` cross-compiles for
//! `thumbv7em-none-eabihf`. But "compiles" is not "works" — until a
//! real backend satisfies the trait surface against an actual `no_std`
//! network stack, the trait surface is unverified. This crate is the
//! verification: an end-to-end working backend that bare-metal Rust
//! consumers can either depend on directly or treat as the worked
//! example for their own (lwIP, smoltcp-direct, vendor-stack) adapters.
//!
//! # Status
//!
//! Phase 19 in progress (per `bare_metal_plan_v3.md`). 19a (this
//! commit) is the scaffold; 19b implements [`EmbassyNetFactory`],
//! 19c implements [`EmbassyNetSocket`], 19e wires up the loopback
//! integration test, 19f produces an in-tree example.
//!
//! # Pairing with `simple-someip`
//!
//! ```toml
//! [dependencies]
//! simple-someip = { version = "0.8", default-features = false,
//!                   features = ["client", "server", "bare_metal"] }
//! simple-someip-embassy-net = "0.1"
//! embassy-net = { version = "0.4", default-features = false,
//!                 features = ["udp", "proto-ipv4", "igmp"] }
//! ```
//!
//! [`simple-someip`]: https://crates.io/crates/simple-someip

#![no_std]
#![warn(clippy::pedantic)]
#![warn(missing_docs)]

pub mod factory;
pub mod socket;

pub use factory::{EmbassyNetFactory, SocketPool};
pub use socket::EmbassyNetSocket;

/// Suggested link-layer MTU for sizing [`SocketPool`] RX/TX buffers
/// and matching driver `Capabilities::max_transmission_unit`.
///
/// 1500 is the canonical Ethernet MTU and the default
/// [`simple_someip::UDP_BUFFER_SIZE`] also lands at 1500. Sizing
/// `SocketPool<_, RX, TX>` with `RX = TX = LINK_MTU` is the
/// configuration these docs assume; smaller values risk dropping
/// full-MTU datagrams at the embassy-net layer (see `SocketPool`
/// for details). Distinct from
/// [`simple_someip::UDP_BUFFER_SIZE`] because that constant is the
/// *application*-payload cap and this one is the *link-layer*
/// frame cap — they coincide at 1500 today but the concepts are
/// orthogonal.
pub const LINK_MTU: usize = 1500;
