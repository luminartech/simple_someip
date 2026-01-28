//! # Simple SOME/IP (Scalable service-Oriented Middleware over IP)
//!
//! SOME/IP is an automotive/embedded communication protocol which supports remote procedure calls,
//! event notifications and the underlying serialization/wire format.
//!
//! This library attempts to expose an ergonomic API for communicating over SOME/IP.
//! This includes encoding/decoding messages, handling the underlying transport,
//! and providing traits to kickstart the development of client/server applications.
//!
//! This library is based on the R23-11 release of the SOME/IP specification which is part of the AUTOSAR standard.
//! This project is not affiliated with the AUTOSAR organization.
//!
//! ## Design
//!
//! ## References
//!
//! ![AUTOSAR Logo](../autosar_logo.svg)
//!
//! - [SOME/IP Specification R23-11](https://www.autosar.org/fileadmin/standards/R23-11/FO/AUTOSAR_FO_PRS_SOMEIPProtocol.pdf)
//! - [AUTOSAR Website](https://www.autosar.org/)

/// Bit flag in message_type field indicating that the message is a SOME/IP TP message.
pub const MESSAGE_TYPE_TP_FLAG: u8 = 0x20;

pub mod someip;
