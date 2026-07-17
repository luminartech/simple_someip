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
        reboot_flag: sd::RebootFlag,
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
            flags: sd::Flags::new_sd(reboot_flag),
            entries: std::vec![entry],
            options: std::vec![endpoint],
        }
    }

    fn set_reboot_flag(header: &mut VecSdHeader, reboot: sd::RebootFlag) {
        header.flags = sd::Flags::new(bool::from(reboot), header.flags.unicast());
    }

    fn for_each_offered_endpoint<F>(&self, mut f: F)
    where
        F: FnMut(crate::OfferedEndpoint),
    {
        let header = match &self.kind {
            RawPayloadKind::Sd(header) => header,
            RawPayloadKind::Raw(_) => return,
        };
        for entry in &header.entries {
            if let sd::Entry::OfferService(svc) | sd::Entry::StopOfferService(svc) = entry {
                let is_offer = matches!(entry, sd::Entry::OfferService(_));
                let endpoint =
                    sd::extract_ipv4_endpoint(&header.options).map(|(addr, protocol)| {
                        crate::NetEndpoint::new(core::net::SocketAddr::V4(addr), protocol)
                    });
                f(crate::OfferedEndpoint {
                    service_id: svc.service_id,
                    instance_id: svc.instance_id,
                    major_version: svc.major_version,
                    minor_version: svc.minor_version,
                    endpoint,
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
            RawPayloadKind::Sd(header) => header,
            RawPayloadKind::Raw(_) => return,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::WireFormat;
    use std::net::Ipv4Addr;

    fn make_sd_payload() -> RawPayload {
        let header = VecSdHeader {
            flags: sd::Flags::new_sd(sd::RebootFlag::RecentlyRebooted),
            entries: std::vec![],
            options: std::vec![],
        };
        RawPayload::new_sd_payload(&header)
    }

    fn make_raw_payload() -> RawPayload {
        let mid = MessageId::new_from_service_and_method(0x1234, 0x0001);
        RawPayload::from_payload_bytes(mid, &[0xDE, 0xAD]).unwrap()
    }

    #[test]
    fn set_reboot_flag_flips_reboot_and_preserves_unicast() {
        // Start with RecentlyRebooted + unicast=true (the `new_sd` preset).
        let mut header = VecSdHeader {
            flags: sd::Flags::new_sd(sd::RebootFlag::RecentlyRebooted),
            entries: std::vec![],
            options: std::vec![],
        };
        assert_eq!(header.flags.reboot(), sd::RebootFlag::RecentlyRebooted);
        assert!(header.flags.unicast());

        RawPayload::set_reboot_flag(&mut header, sd::RebootFlag::Continuous);
        assert_eq!(header.flags.reboot(), sd::RebootFlag::Continuous);
        assert!(
            header.flags.unicast(),
            "unicast bit must not be disturbed by set_reboot_flag"
        );

        RawPayload::set_reboot_flag(&mut header, sd::RebootFlag::RecentlyRebooted);
        assert_eq!(header.flags.reboot(), sd::RebootFlag::RecentlyRebooted);
        assert!(header.flags.unicast());
    }

    #[test]
    fn set_reboot_flag_preserves_cleared_unicast() {
        // Unicast-cleared headers are unusual but legal; set_reboot_flag
        // must not flip unicast back on.
        let mut header = VecSdHeader {
            flags: sd::Flags::new(true, false),
            entries: std::vec![],
            options: std::vec![],
        };
        RawPayload::set_reboot_flag(&mut header, sd::RebootFlag::Continuous);
        assert_eq!(header.flags.reboot(), sd::RebootFlag::Continuous);
        assert!(!header.flags.unicast());
    }

    #[test]
    fn raw_bytes_returns_some_for_raw_payload() {
        let p = make_raw_payload();
        assert_eq!(p.raw_bytes(), Some(&[0xDE, 0xAD][..]));
    }

    #[test]
    fn raw_bytes_returns_none_for_sd_payload() {
        let p = make_sd_payload();
        assert_eq!(p.raw_bytes(), None);
    }

    #[test]
    fn as_sd_header_returns_some_for_sd() {
        let p = make_sd_payload();
        assert!(p.as_sd_header().is_some());
    }

    #[test]
    fn as_sd_header_returns_none_for_raw() {
        let p = make_raw_payload();
        assert!(p.as_sd_header().is_none());
    }

    #[test]
    fn sd_flags_returns_some_for_sd() {
        let p = make_sd_payload();
        let flags = p.sd_flags().unwrap();
        assert!(flags.unicast());
    }

    #[test]
    fn sd_flags_returns_none_for_raw() {
        let p = make_raw_payload();
        assert!(p.sd_flags().is_none());
    }

    #[test]
    fn message_id_correct() {
        let p = make_raw_payload();
        assert_eq!(p.message_id().service_id(), 0x1234);

        let sd = make_sd_payload();
        assert_eq!(sd.message_id(), MessageId::SD);
    }

    #[test]
    fn required_size_raw() {
        let p = make_raw_payload();
        assert_eq!(p.required_size(), 2);
    }

    #[test]
    fn encode_raw_payload() {
        let p = make_raw_payload();
        let mut buf = std::vec![0u8; p.required_size()];
        let n = p.encode(&mut buf.as_mut_slice()).unwrap();
        assert_eq!(n, 2);
        assert_eq!(&buf, &[0xDE, 0xAD]);
    }

    #[test]
    fn encode_sd_payload() {
        let p = make_sd_payload();
        let mut buf = std::vec![0u8; p.required_size()];
        let n = p.encode(&mut buf.as_mut_slice()).unwrap();
        assert_eq!(n, p.required_size());
    }

    #[test]
    fn from_payload_bytes_sd_roundtrip() {
        // Build an SD header with an entry, encode it, then parse it back
        let entry = sd::Entry::FindService(sd::ServiceEntry::find(0x5B));
        let entries = [entry];
        let header = sd::Header::new(
            sd::Flags::new_sd(sd::RebootFlag::RecentlyRebooted),
            &entries,
            &[],
        );
        let mut buf = std::vec![0u8; header.required_size()];
        header.encode(&mut buf.as_mut_slice()).unwrap();

        let p = RawPayload::from_payload_bytes(MessageId::SD, &buf).unwrap();
        assert!(p.as_sd_header().is_some());
        let sd = p.as_sd_header().unwrap();
        assert_eq!(sd.entries.len(), 1);
    }

    #[test]
    fn from_payload_bytes_non_sd() {
        let mid = MessageId::new_from_service_and_method(0x5B, 0x01);
        let p = RawPayload::from_payload_bytes(mid, &[1, 2, 3]).unwrap();
        assert_eq!(p.raw_bytes(), Some(&[1, 2, 3][..]));
    }

    #[test]
    fn new_subscription_sd_header_structure() {
        let header = RawPayload::new_subscription_sd_header(
            0x5B,
            1,
            1,
            3,
            0x01,
            Ipv4Addr::LOCALHOST,
            sd::TransportProtocol::Udp,
            12345,
            sd::RebootFlag::Continuous,
        );
        assert_eq!(header.entries.len(), 1);
        assert_eq!(header.options.len(), 1);
        assert!(header.flags.unicast());
        assert_eq!(header.flags.reboot(), sd::RebootFlag::Continuous);

        let header_reboot = RawPayload::new_subscription_sd_header(
            0x5B,
            1,
            1,
            3,
            0x01,
            Ipv4Addr::LOCALHOST,
            sd::TransportProtocol::Udp,
            12345,
            sd::RebootFlag::RecentlyRebooted,
        );
        assert_eq!(
            header_reboot.flags.reboot(),
            sd::RebootFlag::RecentlyRebooted
        );
    }

    #[test]
    fn offered_endpoints_from_raw_returns_empty() {
        let p = make_raw_payload();
        assert!(p.offered_endpoints().is_empty());
    }

    fn make_offer_entry(service_id: u16, instance_id: u16) -> sd::ServiceEntry {
        sd::ServiceEntry {
            index_first_options_run: 0,
            index_second_options_run: 0,
            options_count: sd::OptionsCount::new(1, 0),
            service_id,
            instance_id,
            major_version: 1,
            ttl: 100,
            minor_version: 0,
        }
    }

    #[test]
    fn offered_endpoints_with_offer_service() {
        let offer = sd::Entry::OfferService(make_offer_entry(0x5B, 1));
        let endpoint = sd::Options::IpV4Endpoint {
            ip: Ipv4Addr::LOCALHOST,
            protocol: sd::TransportProtocol::Udp,
            port: 30000,
        };
        let header = VecSdHeader {
            flags: sd::Flags::new_sd(sd::RebootFlag::RecentlyRebooted),
            entries: std::vec![offer],
            options: std::vec![endpoint],
        };
        let p = RawPayload::new_sd_payload(&header);
        let endpoints = p.offered_endpoints();
        assert_eq!(endpoints.len(), 1);
        assert_eq!(endpoints[0].service_id, 0x5B);
        assert!(endpoints[0].is_offer);
        let ep = endpoints[0].endpoint.expect("endpoint option present");
        assert_eq!(ep.protocol, crate::TransportProtocol::Udp);
        assert_eq!(
            ep.addr,
            core::net::SocketAddr::V4(core::net::SocketAddrV4::new(Ipv4Addr::LOCALHOST, 30000))
        );
    }

    #[test]
    fn offered_endpoints_with_stop_offer() {
        let mut entry = make_offer_entry(0x5B, 1);
        entry.ttl = 0;
        let stop = sd::Entry::StopOfferService(entry);
        let header = VecSdHeader {
            flags: sd::Flags::new_sd(sd::RebootFlag::RecentlyRebooted),
            entries: std::vec![stop],
            options: std::vec![],
        };
        let p = RawPayload::new_sd_payload(&header);
        let endpoints = p.offered_endpoints();
        assert_eq!(endpoints.len(), 1);
        assert!(!endpoints[0].is_offer);
        assert!(endpoints[0].endpoint.is_none());
    }

    #[test]
    fn offered_endpoints_ignores_non_offer_entries() {
        let find = sd::Entry::FindService(sd::ServiceEntry::find(0x5B));
        let header = VecSdHeader {
            flags: sd::Flags::new_sd(sd::RebootFlag::RecentlyRebooted),
            entries: std::vec![find],
            options: std::vec![],
        };
        let p = RawPayload::new_sd_payload(&header);
        assert!(p.offered_endpoints().is_empty());
    }

    #[test]
    fn service_instances_returns_all_entry_types() {
        let offer = sd::Entry::OfferService(make_offer_entry(0x47, 1));
        let find = sd::Entry::FindService(sd::ServiceEntry::find(0x5D));
        let sub = sd::Entry::SubscribeEventGroup(sd::EventGroupEntry::new(0x5B, 1, 1, 3, 0x01));
        let header = VecSdHeader {
            flags: sd::Flags::new_sd(sd::RebootFlag::RecentlyRebooted),
            entries: std::vec![offer, find, sub],
            options: std::vec![],
        };
        let p = RawPayload::new_sd_payload(&header);
        let instances = p.service_instances();
        assert_eq!(instances.len(), 3);
        assert_eq!(instances[0], (0x47, 1)); // OfferService
        assert_eq!(instances[1], (0x5D, 0xFFFF)); // FindService (wildcard instance)
        assert_eq!(instances[2], (0x5B, 1)); // SubscribeEventGroup
    }

    #[test]
    fn service_instances_empty_for_raw_payload() {
        let mid = MessageId::new_from_service_and_method(0x5B, 0x01);
        let p = RawPayload::from_payload_bytes(mid, &[1, 2, 3]).unwrap();
        assert!(p.service_instances().is_empty());
    }

    #[test]
    fn service_instances_empty_for_no_entries() {
        let header = VecSdHeader {
            flags: sd::Flags::new_sd(sd::RebootFlag::RecentlyRebooted),
            entries: std::vec![],
            options: std::vec![],
        };
        let p = RawPayload::new_sd_payload(&header);
        assert!(p.service_instances().is_empty());
    }

    #[test]
    fn vec_sd_header_required_size_and_encode() {
        let header = VecSdHeader {
            flags: sd::Flags::new_sd(sd::RebootFlag::RecentlyRebooted),
            entries: std::vec![],
            options: std::vec![],
        };
        let size = header.required_size();
        assert!(size > 0);
        let mut buf = std::vec![0u8; size];
        let n = header.encode(&mut buf.as_mut_slice()).unwrap();
        assert_eq!(n, size);
    }
}
