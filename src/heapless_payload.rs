//! A no_std, alloc-free [`PayloadWireFormat`] implementation.
//!
//! Mirrors the std-only `RawPayload` but swaps every `Vec`
//! for a `heapless::Vec<_, CAP>` so the type is usable on targets
//! without an allocator. Suitable as the `MessageDefinitions`
//! parameter for `Client<MessageDefinitions, _, _>` /
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
//! - `OPT_CAP` = 8: typically 1–2 IpV4Endpoint options per entry. A
//!   received option that no entry references takes no slot.
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
use crate::traits::PayloadWireFormat;
use automotive_wire_codec::Encode;

/// Max SD entries in a single payload. See module-level docs.
pub const ENTRY_CAP: usize = 8;
/// Max SD options in a single payload, counting only those its entries
/// reference. See module-level docs.
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
    /// The SD options the entries reference, in order. Each entry's option
    /// runs index into this list.
    pub options: HVec<sd::Options, OPT_CAP>,
}

impl HeaplessSdHeader {
    /// This header as an [`sd::Header`] borrowing its entries and options,
    /// for example to walk [`sd::Header::entries_with_options`].
    #[must_use]
    pub fn as_header(&self) -> sd::Header<'_> {
        sd::Header::new(self.flags, &self.entries, &self.options)
    }
}

impl Encode for HeaplessSdHeader {
    type Error = protocol::Error;

    fn encoded_size(&self) -> Result<usize, Self::Error> {
        self.as_header().encoded_size()
    }

    fn encode(&self, writer: &mut impl embedded_io::Write) -> Result<usize, protocol::Error> {
        self.as_header().encode(writer)
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

impl Encode for HeaplessPayload {
    type Error = protocol::Error;

    fn encoded_size(&self) -> Result<usize, Self::Error> {
        match &self.kind {
            HeaplessPayloadKind::Sd(header) => header.encoded_size(),
            HeaplessPayloadKind::Raw(bytes) => Ok(bytes.len()),
        }
    }

    fn encode(&self, writer: &mut impl embedded_io::Write) -> Result<usize, Self::Error> {
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
            let options = keep_referenced_options(&view, &mut entries)?;
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

    fn for_each_service_offer<'a, F>(&'a self, f: F)
    where
        F: FnMut(crate::ServiceOffer<'a>),
    {
        let header = match &self.kind {
            HeaplessPayloadKind::Sd(header) => header,
            HeaplessPayloadKind::Raw(_) => return,
        };
        crate::traits::for_each_service_offer(header.as_header(), f);
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

/// The options of `view` that `entries` reference, in order, with each entry's
/// option runs moved to index into them.
fn keep_referenced_options(
    view: &sd::SdHeaderView<'_>,
    entries: &mut [sd::Entry],
) -> Result<HVec<sd::Options, OPT_CAP>, protocol::Error> {
    fn out_of_memory<T>(_: T) -> protocol::Error {
        protocol::Error::Io(embedded_io::ErrorKind::OutOfMemory)
    }
    let mut options = HVec::new();
    let mut kept: HVec<usize, OPT_CAP> = HVec::new();
    for (index, option) in view.options().enumerate() {
        if entries
            .iter()
            .any(|entry| entry.option_runs().covers(index))
        {
            options.push(option.to_owned()?).map_err(out_of_memory)?;
            kept.push(index).map_err(out_of_memory)?;
        }
    }
    // `kept` ascends, so a run's new start is the number of kept options
    // before its old one. Every option a run covers is kept, so its count holds.
    for entry in entries {
        entry.move_option_runs(|start| {
            kept.iter()
                .take_while(|&&index| index < usize::from(start))
                .fold(0, |before, _| before + 1)
        });
    }
    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::sd::test_support::{
        UNKNOWN_OPTION_TYPE, endpoint_after_unknown_option, offer_after_unknown_entry,
        sd_body_with_unknown_entry_and_option,
    };

    /// An Offer of service 0x1234 whose first run is `first` and second run
    /// is `second`, each a start index and a count.
    fn offer(first: (u8, u8), second: (u8, u8)) -> sd::Entry {
        sd::Entry::OfferService(sd::ServiceEntry {
            index_first_options_run: first.0,
            index_second_options_run: second.0,
            options_count: sd::OptionsCount::new(first.1, second.1),
            service_id: 0x1234,
            instance_id: 1,
            major_version: 1,
            ttl: 3,
            minor_version: 0,
        })
    }

    /// An IPv4 UDP endpoint option on 192.0.2.1 at `port`, encoded.
    fn endpoint(port: u16) -> [u8; 12] {
        let mut bytes = [0u8; 12];
        sd::Options::IpV4Endpoint {
            ip: core::net::Ipv4Addr::new(192, 0, 2, 1),
            protocol: sd::TransportProtocol::Udp,
            port,
        }
        .encode(&mut bytes.as_mut_slice())
        .unwrap();
        bytes
    }

    /// An option of type [`UNKNOWN_OPTION_TYPE`] with a 4-byte body.
    fn unknown_option(discardable: bool) -> [u8; 8] {
        let flags = if discardable { 0x80 } else { 0x00 };
        [0x00, 0x05, UNKNOWN_OPTION_TYPE, flags, 1, 2, 3, 4]
    }

    /// An SD body carrying `entries`, then `options` in order.
    fn sd_body(entries: &[sd::Entry], options: &[&[u8]]) -> HVec<u8, 512> {
        let mut body = HVec::new();
        body.extend_from_slice(&[0xC0, 0, 0, 0]).unwrap();
        let entries_length = u32::try_from(entries.len() * 16).unwrap();
        body.extend_from_slice(&entries_length.to_be_bytes())
            .unwrap();
        for entry in entries {
            let mut bytes = [0u8; 16];
            entry.encode(&mut bytes.as_mut_slice()).unwrap();
            body.extend_from_slice(&bytes).unwrap();
        }
        let options_length = u32::try_from(options.iter().map(|o| o.len()).sum::<usize>()).unwrap();
        body.extend_from_slice(&options_length.to_be_bytes())
            .unwrap();
        for option in options {
            body.extend_from_slice(option).unwrap();
        }
        body
    }

    type EntryOptions = HVec<HVec<sd::Options, 16>, ENTRY_CAP>;

    /// The options each entry of `body` references, read off the wire.
    fn wire_entry_options(body: &[u8]) -> EntryOptions {
        let view = sd::SdHeaderView::parse(body).unwrap();
        view.entries_with_options()
            .map(|(_, options)| options.map(|o| o.to_owned().unwrap()).collect())
            .collect()
    }

    /// The options each entry of `payload` references.
    fn entry_options(payload: &HeaplessPayload) -> EntryOptions {
        let header = payload.as_sd_header().unwrap();
        header
            .as_header()
            .entries_with_options()
            .map(|(_, options)| options.cloned().collect())
            .collect()
    }

    #[test]
    fn sd_payload_skips_unknown_entries_and_unreferenced_options() {
        let body = sd_body_with_unknown_entry_and_option();
        let payload = HeaplessPayload::from_payload_bytes(MessageId::SD, &body)
            .expect("unknown types do not reject the message");
        let header = payload.as_sd_header().unwrap();
        assert_eq!(header.options.as_slice(), [endpoint_after_unknown_option()]);
        let mut offer = offer_after_unknown_entry();
        offer.move_option_runs(|_| 0);
        assert_eq!(header.entries.as_slice(), [offer]);
        assert_eq!(entry_options(&payload), wire_entry_options(&body));
    }

    #[test]
    fn offer_applies_when_unreferenced_options_exceed_opt_cap() {
        let unknown = unknown_option(true);
        let (first, second) = (endpoint(30509), endpoint(30510));
        let mut options: HVec<&[u8], 16> = HVec::new();
        for _ in 0..7 {
            options.push(&unknown).unwrap();
        }
        options.push(&first).unwrap();
        options.push(&second).unwrap();
        assert!(options.len() > OPT_CAP);
        let body = sd_body(&[offer((7, 2), (0, 0))], &options);

        let payload = HeaplessPayload::from_payload_bytes(MessageId::SD, &body)
            .expect("only the referenced options count against OPT_CAP");

        assert_eq!(payload.as_sd_header().unwrap().options.len(), 2);
        assert_eq!(entry_options(&payload), wire_entry_options(&body));
        let mut offered = 0;
        payload.for_each_service_offer(|offer| {
            assert!(matches!(offer, crate::ServiceOffer::Offer { .. }));
            offered += 1;
        });
        assert_eq!(offered, 1);
    }

    #[test]
    fn referenced_unknown_option_is_kept_in_both_runs() {
        let (dropped, kept) = (unknown_option(true), unknown_option(false));
        let (first, second) = (endpoint(30509), endpoint(30510));
        let body = sd_body(
            &[offer((1, 2), (4, 1)), offer((4, 1), (0, 0))],
            &[&dropped, &first, &kept, &dropped, &second],
        );

        let payload = HeaplessPayload::from_payload_bytes(MessageId::SD, &body).unwrap();

        assert_eq!(payload.as_sd_header().unwrap().options.len(), 3);
        let options = entry_options(&payload);
        assert_eq!(options, wire_entry_options(&body));
        assert!(matches!(
            options[0][1],
            sd::Options::Unknown {
                discardable: false,
                ..
            }
        ));
    }

    #[test]
    fn runs_past_the_last_option_reference_the_same_options() {
        let unknown = unknown_option(true);
        let first = endpoint(30509);
        let body = sd_body(&[offer((1, 3), (5, 1))], &[&unknown, &first]);

        let payload = HeaplessPayload::from_payload_bytes(MessageId::SD, &body).unwrap();

        assert_eq!(entry_options(&payload), wire_entry_options(&body));
    }

    #[test]
    fn more_referenced_options_than_opt_cap_is_out_of_memory() {
        let first = endpoint(30509);
        let options = [&first[..]; OPT_CAP + 1];
        let count = u8::try_from(OPT_CAP + 1).unwrap();
        let body = sd_body(&[offer((0, count), (0, 0))], &options);

        let result = HeaplessPayload::from_payload_bytes(MessageId::SD, &body);

        assert!(matches!(
            result,
            Err(protocol::Error::Io(embedded_io::ErrorKind::OutOfMemory))
        ));
    }
}
