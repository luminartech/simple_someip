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

#![warn(clippy::pedantic)]
// TODO: Add `# Errors` and `# Panics` doc sections in a follow-up PR.
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]

#[cfg(feature = "client")]
mod client;
pub mod e2e;
mod error;
pub mod protocol;
#[cfg(feature = "server")]
pub mod server;
pub mod traits;

#[cfg(feature = "client")]
pub use client::*;
pub use error::Error;
#[cfg(feature = "server")]
pub use server::Server;

use core::net::Ipv4Addr;

pub const SD_MULTICAST_IP: Ipv4Addr = Ipv4Addr::new(239, 255, 0, 255);
pub const SD_MULTICAST_PORT: u16 = 30490;
///Message id for SOME/IP service discovery messages
pub const SD_MESSAGE_ID_VALUE: u32 = 0xffff_8100;
