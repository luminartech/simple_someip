//! A general-purpose, heap-allocated [`PayloadWireFormat`] implementation.
//!
//! [`VecSdHeader`] stores SD entries and options in `Vec`s (instead of
//! fixed-capacity `heapless::Vec`s), and [`RawPayload`] wraps either an
//! SD header or opaque bytes so that `Message<RawPayload>` can represent
//! *any* SOME/IP message without a custom payload type.
//!
//! This module is only available when the **`std`** feature is enabled.

use std::vec::Vec;

use embedded_io::Error as _;

use crate::protocol::{self, MessageId, sd};
use crate::traits::{PayloadWireFormat, WireFormat};

/// Owned SD header backed by heap-allocated vectors.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VecSdHeader {
    /// SD flags byte.
    pub flags: sd::Flags,
    /// SD entries.
    pub entries: Vec<sd::Entry>,
    /// SD options.
    pub options: Vec<sd::Options>,
}

impl WireFormat for VecSdHeader {
    fn required_size(&self) -> usize {
        sd::Header::new(self.flags, &self.entries, &self.options).required_size()
    }

    fn encode<T: embedded_io::Write>(&self, writer: &mut T) -> Result<usize, protocol::Error> {
        sd::Header::new(self.flags, &self.entries, &self.options).encode(writer)
    }
}

/// The inner representation of a [`RawPayload`].
#[derive(Clone, Debug, Eq, PartialEq)]
enum RawPayloadKind {
    /// Service-discovery payload.
    Sd(VecSdHeader),
    /// Opaque byte payload for any non-SD message.
    Raw(Vec<u8>),
}

/// A concrete [`PayloadWireFormat`] backed by heap-allocated storage.
///
/// SD messages are stored as a [`VecSdHeader`]; all other messages are
/// stored as opaque bytes.  This type is suitable as the payload parameter
/// for `Message<RawPayload>` in FFI bindings or any context where a
/// fixed, non-generic payload type is needed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawPayload {
    message_id: MessageId,
    kind: RawPayloadKind,
}

impl RawPayload {
    /// Returns the raw payload bytes for non-SD messages, or `None` for SD messages.
    #[must_use]
    pub fn raw_bytes(&self) -> Option<&[u8]> {
        match &self.kind {
            RawPayloadKind::Raw(bytes) => Some(bytes),
            RawPayloadKind::Sd(_) => None,
        }
    }
}

impl PayloadWireFormat for RawPayload {
    type SdHeader = VecSdHeader;

    fn message_id(&self) -> MessageId {
        self.message_id
    }

    fn as_sd_header(&self) -> Option<&VecSdHeader> {
        match &self.kind {
            RawPayloadKind::Sd(header) => Some(header),
            RawPayloadKind::Raw(_) => None,
        }
    }

    fn from_payload_bytes(message_id: MessageId, payload: &[u8]) -> Result<Self, protocol::Error> {
        if message_id == MessageId::SD {
            let view = sd::SdHeaderView::parse(payload)?;
            let mut entries = Vec::new();
            for ev in view.entries() {
                entries.push(ev.to_owned()?);
            }
            let mut options = Vec::new();
            for ov in view.options() {
                options.push(ov.to_owned()?);
            }
            Ok(Self {
                message_id,
                kind: RawPayloadKind::Sd(VecSdHeader {
                    flags: view.flags(),
                    entries,
                    options,
                }),
            })
        } else {
            Ok(Self {
                message_id,
                kind: RawPayloadKind::Raw(payload.to_vec()),
            })
        }
    }

    fn new_sd_payload(header: &VecSdHeader) -> Self {
        Self {
            message_id: MessageId::SD,
            kind: RawPayloadKind::Sd(header.clone()),
        }
    }

    fn sd_flags(&self) -> Option<sd::Flags> {
        match &self.kind {
            RawPayloadKind::Sd(header) => Some(header.flags),
            RawPayloadKind::Raw(_) => None,
        }
    }

    fn required_size(&self) -> usize {
        match &self.kind {
            RawPayloadKind::Sd(header) => header.required_size(),
            RawPayloadKind::Raw(bytes) => bytes.len(),
        }
    }

    fn encode<T: embedded_io::Write>(&self, writer: &mut T) -> Result<usize, protocol::Error> {
        match &self.kind {
            RawPayloadKind::Sd(header) => header.encode(writer),
            RawPayloadKind::Raw(bytes) => {
                writer
                    .write_all(bytes)
                    .map_err(|e| protocol::Error::Io(e.kind()))?;
                Ok(bytes.len())
            }
        }
    }

    fn new_subscription_sd_header(
        service_id: u16,
        instance_id: u16,
        major_version: u8,
        ttl: u32,
        event_group_id: u16,
        client_ip: std::net::Ipv4Addr,
        protocol: sd::TransportProtocol,
        client_port: u16,
    ) -> VecSdHeader {
        let entry = sd::Entry::SubscribeEventGroup(sd::EventGroupEntry::new(
            service_id,
            instance_id,
            major_version,
            ttl,
            event_group_id,
        ));
        let endpoint = sd::Options::IpV4Endpoint {
            ip: client_ip,
            protocol,
            port: client_port,
        };
        VecSdHeader {
            flags: sd::Flags::new_sd(false),
            entries: std::vec![entry],
            options: std::vec![endpoint],
        }
    }

    fn offered_endpoints(&self) -> Vec<crate::OfferedEndpoint> {
        let header = match &self.kind {
            RawPayloadKind::Sd(header) => header,
            RawPayloadKind::Raw(_) => return Vec::new(),
        };
        header
            .entries
            .iter()
            .filter_map(|entry| match entry {
                sd::Entry::OfferService(svc) | sd::Entry::StopOfferService(svc) => {
                    let is_offer = matches!(entry, sd::Entry::OfferService(_));
                    let addr = sd::extract_ipv4_endpoint(&header.options);
                    Some(crate::OfferedEndpoint {
                        service_id: svc.service_id,
                        instance_id: svc.instance_id,
                        major_version: svc.major_version,
                        minor_version: svc.minor_version,
                        addr,
                        is_offer,
                    })
                }
                _ => None,
            })
            .collect()
    }
}
