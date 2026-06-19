//! A no_std, alloc-free [`PayloadWireFormat`] implementation.
//!
//! Mirrors the std-only `RawPayload` but swaps every `Vec`
//! for a `heapless::Vec<_, CAP>` so the type is usable on targets
//! without an allocator. Suitable as the `MessageDefinitions`
//! parameter for `Client<MessageDefinitions, _, _, _>` /
//! `Message<MessageDefinitions>` in bare-metal firmware
//! (`embassy_executor` + static `define_static_channels!`-backed
//! channels).
//!
//! ## Capacity bounds
//!
//! The fixed caps balance worst-case SD-burst absorption against
//! static memory cost:
//! - `ENTRY_CAP` = 8: a single SD datagram rarely carries more
//!   than 2–3 entries in practice; 8 covers vsomeip-class
//!   peers that fold multiple OfferService / SubscribeAck into one
//!   datagram.
//! - `OPT_CAP` = 8: typically 1–2 IpV4Endpoint options per entry.
//! - `PAYLOAD_CAP` = 2048: matches the application-level UDP cap
//!   most firmware targets land on (`UDP_BUFFER_SIZE`).
//!
//! Bump any of these if your peer's traffic shape requires it —
//! storage costs scale linearly. See module body for the exact
//! sizing footprint.
//!
//! This module is only compiled when the **`bare_metal`** feature is
//! enabled.

use embedded_io::Error as _;
use heapless::Vec as HVec;

use crate::protocol::{self, MessageId, sd};
use crate::traits::{PayloadWireFormat, WireFormat};

/// Max SD entries in a single payload. See module-level docs.
pub const ENTRY_CAP: usize = 8;
/// Max SD options in a single payload. See module-level docs.
pub const OPT_CAP: usize = 8;
/// Max raw (non-SD) payload byte length. See module-level docs.
/// Halo / bare-metal tight-BSS sizing: 256 covers halo's expected
/// inbound payload sizes and keeps `HeaplessPayload::Raw` from
/// bloating `BoundedPooled` channel slots.
pub const PAYLOAD_CAP: usize = 256;

/// Owned SD header backed by heapless vectors.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeaplessSdHeader {
    /// SD flags byte.
    pub flags: sd::Flags,
    /// SD entries.
    pub entries: HVec<sd::Entry, ENTRY_CAP>,
    /// SD options.
    pub options: HVec<sd::Options, OPT_CAP>,
}

impl WireFormat for HeaplessSdHeader {
    fn required_size(&self) -> usize {
        sd::Header::new(self.flags, &self.entries, &self.options).required_size()
    }

    fn encode<T: embedded_io::Write>(&self, writer: &mut T) -> Result<usize, protocol::Error> {
        sd::Header::new(self.flags, &self.entries, &self.options).encode(writer)
    }
}

/// Inner representation of [`HeaplessPayload`].
// The `Raw` variant inlines a `PAYLOAD_CAP`-byte buffer, dwarfing the `Sd`
// variant — but this is a no-alloc target, so boxing the large variant
// (clippy's usual remedy) is not an option. The inline size is intentional.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Eq, PartialEq)]
enum HeaplessPayloadKind {
    /// Service-discovery payload.
    Sd(HeaplessSdHeader),
    /// Opaque byte payload for any non-SD message.
    Raw(HVec<u8, PAYLOAD_CAP>),
}

/// `no_std` / no-alloc concrete [`PayloadWireFormat`]. Counterpart of
/// the std-only `RawPayload` for bare-metal targets.
///
/// SD messages are stored as a [`HeaplessSdHeader`]; all other
/// messages are stored as opaque bytes in a `heapless::Vec`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeaplessPayload {
    message_id: MessageId,
    kind: HeaplessPayloadKind,
}

impl HeaplessPayload {
    /// Returns the raw payload bytes for non-SD messages, or `None`
    /// for SD messages.
    #[must_use]
    pub fn raw_bytes(&self) -> Option<&[u8]> {
        match &self.kind {
            HeaplessPayloadKind::Raw(bytes) => Some(bytes),
            HeaplessPayloadKind::Sd(_) => None,
        }
    }
}

impl PayloadWireFormat for HeaplessPayload {
    type SdHeader = HeaplessSdHeader;

    fn message_id(&self) -> MessageId {
        self.message_id
    }

    fn as_sd_header(&self) -> Option<&HeaplessSdHeader> {
        match &self.kind {
            HeaplessPayloadKind::Sd(header) => Some(header),
            HeaplessPayloadKind::Raw(_) => None,
        }
    }

    fn from_payload_bytes(message_id: MessageId, payload: &[u8]) -> Result<Self, protocol::Error> {
        if message_id == MessageId::SD {
            let view = sd::SdHeaderView::parse(payload)?;
            let mut entries: HVec<sd::Entry, ENTRY_CAP> = HVec::new();
            for ev in view.entries() {
                let entry = ev.to_owned()?;
                entries
                    .push(entry)
                    .map_err(|_| protocol::Error::Io(embedded_io::ErrorKind::OutOfMemory))?;
            }
            let mut options: HVec<sd::Options, OPT_CAP> = HVec::new();
            for ov in view.options() {
                let opt = ov.to_owned()?;
                options
                    .push(opt)
                    .map_err(|_| protocol::Error::Io(embedded_io::ErrorKind::OutOfMemory))?;
            }
            Ok(Self {
                message_id,
                kind: HeaplessPayloadKind::Sd(HeaplessSdHeader {
                    flags: view.flags(),
                    entries,
                    options,
                }),
            })
        } else {
            let mut bytes: HVec<u8, PAYLOAD_CAP> = HVec::new();
            bytes
                .extend_from_slice(payload)
                .map_err(|_| protocol::Error::Io(embedded_io::ErrorKind::OutOfMemory))?;
            Ok(Self {
                message_id,
                kind: HeaplessPayloadKind::Raw(bytes),
            })
        }
    }

    fn new_sd_payload(header: &HeaplessSdHeader) -> Self {
        Self {
            message_id: MessageId::SD,
            kind: HeaplessPayloadKind::Sd(header.clone()),
        }
    }

    fn sd_flags(&self) -> Option<sd::Flags> {
        match &self.kind {
            HeaplessPayloadKind::Sd(header) => Some(header.flags),
            HeaplessPayloadKind::Raw(_) => None,
        }
    }

    fn required_size(&self) -> usize {
        match &self.kind {
            HeaplessPayloadKind::Sd(header) => header.required_size(),
            HeaplessPayloadKind::Raw(bytes) => bytes.len(),
        }
    }

    fn encode<T: embedded_io::Write>(&self, writer: &mut T) -> Result<usize, protocol::Error> {
        match &self.kind {
            HeaplessPayloadKind::Sd(header) => header.encode(writer),
            HeaplessPayloadKind::Raw(bytes) => {
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
        client_ip: core::net::Ipv4Addr,
        protocol: sd::TransportProtocol,
        client_port: u16,
        reboot_flag: sd::RebootFlag,
    ) -> HeaplessSdHeader {
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
        let mut entries: HVec<sd::Entry, ENTRY_CAP> = HVec::new();
        let _ = entries.push(entry); // cap >= 1, never fails
        let mut options: HVec<sd::Options, OPT_CAP> = HVec::new();
        let _ = options.push(endpoint);
        HeaplessSdHeader {
            flags: sd::Flags::new_sd(reboot_flag),
            entries,
            options,
        }
    }

    fn set_reboot_flag(header: &mut HeaplessSdHeader, reboot: sd::RebootFlag) {
        header.flags = sd::Flags::new(bool::from(reboot), header.flags.unicast());
    }

    fn for_each_offered_endpoint<F>(&self, mut f: F)
    where
        F: FnMut(crate::OfferedEndpoint),
    {
        let header = match &self.kind {
            HeaplessPayloadKind::Sd(header) => header,
            HeaplessPayloadKind::Raw(_) => return,
        };
        for entry in &header.entries {
            if let sd::Entry::OfferService(svc) | sd::Entry::StopOfferService(svc) = entry {
                let is_offer = matches!(entry, sd::Entry::OfferService(_));
                let addr = sd::extract_ipv4_endpoint(&header.options);
                f(crate::OfferedEndpoint {
                    service_id: svc.service_id,
                    instance_id: svc.instance_id,
                    major_version: svc.major_version,
                    minor_version: svc.minor_version,
                    addr,
                    is_offer,
                });
            }
        }
    }

    fn for_each_service_instance<F>(&self, mut f: F)
    where
        F: FnMut(u16, u16),
    {
        let header = match &self.kind {
            HeaplessPayloadKind::Sd(header) => header,
            HeaplessPayloadKind::Raw(_) => return,
        };
        for entry in &header.entries {
            let (svc, inst) = match entry {
                sd::Entry::FindService(svc)
                | sd::Entry::OfferService(svc)
                | sd::Entry::StopOfferService(svc) => (svc.service_id, svc.instance_id),
                sd::Entry::SubscribeEventGroup(eg) | sd::Entry::SubscribeAckEventGroup(eg) => {
                    (eg.service_id, eg.instance_id)
                }
            };
            f(svc, inst);
        }
    }
}
