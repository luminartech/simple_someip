//! Pure, no-alloc SOME/IP + SD datagram codec for bare-metal targets.
//!
//! Transport-agnostic builders and parsers: encode `OfferService` /
//! `StopOfferService` / `SubscribeEventgroup` / `SubscribeAckEventgroup`
//! SD datagrams, and parse inbound SOME/IP / SOME/IP-SD datagrams. No
//! async, no sockets, no allocation — callers own the scratch buffer and
//! the transmit path. These back the spawnable futures in
//! [`crate::bare_metal_tasks`] and the firmware's publish/deinit FFI, so
//! that no SOME/IP byte-encoding or header-parsing lives in the firmware.

use core::net::Ipv4Addr;
use core::sync::atomic::{AtomicU16, Ordering};

use crate::WireFormat;
use crate::protocol::sd::{
    Entry, EventGroupEntry, Flags, Header as SdHeader, Options as SdOptions, OptionsCount,
    RebootFlag, SdHeaderView, ServiceEntry, TransportProtocol,
};
use crate::protocol::{Header, HeaderView, MessageId};
use crate::transport::E2ERegistryHandle;
use crate::{E2ECheckStatus, E2EKey};

/// SOME/IP header length in bytes — the offset at which an SD or
/// notification payload begins.
pub const SOMEIP_HEADER_LEN: usize = 16;

/// One `SubscribeEventgroup` entry, with the local endpoint the
/// publisher should deliver events to.
#[derive(Clone, Copy, Debug)]
pub struct SubscribeEventgroupRequest {
    pub service_id: u16,
    pub instance_id: u16,
    pub major_version: u8,
    pub event_group_id: u16,
    /// Subscription TTL in seconds (24-bit).
    pub ttl: u32,
    pub local_ip: Ipv4Addr,
    /// Local UDP port the client receives events on.
    pub local_rx_port: u16,
}

/// One `OfferService` / `StopOfferService` entry, with the local
/// endpoint the service is reachable at.
#[derive(Clone, Copy, Debug)]
pub struct OfferServiceRequest {
    pub service_id: u16,
    pub instance_id: u16,
    pub major_version: u8,
    pub minor_version: u32,
    /// Offer TTL in seconds. [`build_stop_offer_service_datagram`]
    /// forces this to `0`.
    pub ttl: u32,
    pub local_ip: Ipv4Addr,
    /// Local UDP port the service is bound to.
    pub unicast_port: u16,
}

/// One `SubscribeAckEventgroup` entry. Fields echo the inbound
/// `Subscribe`.
#[derive(Clone, Copy, Debug)]
pub struct SubscribeAckRequest {
    pub service_id: u16,
    pub instance_id: u16,
    pub event_group_id: u16,
    pub major_version: u8,
    pub ttl: u32,
}

/// Packet-construction errors.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BuildError {
    /// `buf` is shorter than the encoded datagram.
    BufferTooSmall,
    /// SD or SOME/IP encoding failed mid-write.
    EncodeFailed,
}

/// Encode a `SubscribeEventgroup` datagram into `buf`. Returns its
/// length in bytes. `session` should come from [`next_sd_session`].
///
/// `reboot` sets the SD reboot flag: a freshly-booted client emits
/// [`RebootFlag::RecentlyRebooted`] until its session counter wraps, so
/// the publisher can detect the restart and re-establish the
/// subscription.
///
/// # Errors
/// See [`BuildError`].
pub fn build_subscribe_eventgroup_datagram(
    buf: &mut [u8],
    request: &SubscribeEventgroupRequest,
    session: u16,
    reboot: RebootFlag,
) -> Result<usize, BuildError> {
    let entry = Entry::SubscribeEventGroup(EventGroupEntry {
        index_first_options_run: 0,
        index_second_options_run: 0,
        options_count: OptionsCount::new(1, 0),
        service_id: request.service_id,
        instance_id: request.instance_id,
        major_version: request.major_version,
        ttl: request.ttl,
        counter: 0,
        event_group_id: request.event_group_id,
    });
    let option = SdOptions::IpV4Endpoint {
        ip: request.local_ip,
        port: request.local_rx_port,
        protocol: TransportProtocol::Udp,
    };
    encode_sd_datagram(buf, &[entry], &[option], session, reboot)
}

/// Encode an `OfferService` datagram into `buf`. Returns its length in
/// bytes.
///
/// # Errors
/// See [`BuildError`].
pub fn build_offer_service_datagram(
    buf: &mut [u8],
    request: &OfferServiceRequest,
    session: u16,
) -> Result<usize, BuildError> {
    encode_service_entry_datagram(buf, request, session, /*stop=*/ false)
}

/// Encode a `StopOfferService` datagram into `buf` (same shape as
/// [`build_offer_service_datagram`], TTL forced to `0`).
///
/// # Errors
/// See [`BuildError`].
pub fn build_stop_offer_service_datagram(
    buf: &mut [u8],
    request: &OfferServiceRequest,
    session: u16,
) -> Result<usize, BuildError> {
    encode_service_entry_datagram(buf, request, session, /*stop=*/ true)
}

/// Encode a single SD datagram carrying one `OfferService` entry per
/// element of `requests` (up to `N`). Each entry references its own
/// IPv4 endpoint option at the matching flat index — one coherent SD
/// message instead of one packet per service.
///
/// # Errors
/// See [`BuildError`].
pub fn build_multi_offer_service_datagram<const N: usize>(
    buf: &mut [u8],
    requests: &[OfferServiceRequest],
    session: u16,
) -> Result<usize, BuildError> {
    build_multi_service_entry_datagram::<N>(buf, requests, session, /*stop=*/ false)
}

/// Encode a single SD datagram carrying one `StopOfferService` entry per
/// element of `requests` (up to `N`).
///
/// # Errors
/// See [`build_multi_offer_service_datagram`].
pub fn build_multi_stop_offer_service_datagram<const N: usize>(
    buf: &mut [u8],
    requests: &[OfferServiceRequest],
    session: u16,
) -> Result<usize, BuildError> {
    build_multi_service_entry_datagram::<N>(buf, requests, session, /*stop=*/ true)
}

fn build_multi_service_entry_datagram<const N: usize>(
    buf: &mut [u8],
    requests: &[OfferServiceRequest],
    session: u16,
    stop: bool,
) -> Result<usize, BuildError> {
    let mut entries: heapless::Vec<Entry, N> = heapless::Vec::new();
    let mut options: heapless::Vec<SdOptions, N> = heapless::Vec::new();
    for (i, req) in requests.iter().enumerate().take(N) {
        #[allow(clippy::cast_possible_truncation)]
        let svc = ServiceEntry {
            // Each entry owns exactly one option at position `i` in the
            // flat options array.
            index_first_options_run: i as u8,
            index_second_options_run: 0,
            options_count: OptionsCount::new(1, 0),
            service_id: req.service_id,
            instance_id: req.instance_id,
            major_version: req.major_version,
            ttl: if stop { 0 } else { req.ttl },
            minor_version: req.minor_version,
        };
        let entry = if stop {
            Entry::StopOfferService(svc)
        } else {
            Entry::OfferService(svc)
        };
        entries.push(entry).map_err(|_| BuildError::BufferTooSmall)?;
        options
            .push(SdOptions::IpV4Endpoint {
                ip: req.local_ip,
                port: req.unicast_port,
                protocol: TransportProtocol::Udp,
            })
            .map_err(|_| BuildError::BufferTooSmall)?;
    }
    encode_sd_datagram(buf, &entries, &options, session, RebootFlag::Continuous)
}

/// Encode a `SubscribeAckEventgroup` datagram into `buf`. Caller
/// transmits to the original `Subscribe` sender's SD endpoint.
///
/// # Errors
/// See [`BuildError`].
pub fn build_subscribe_ack_datagram(
    buf: &mut [u8],
    request: &SubscribeAckRequest,
    session: u16,
) -> Result<usize, BuildError> {
    let entry = Entry::SubscribeAckEventGroup(EventGroupEntry {
        index_first_options_run: 0,
        index_second_options_run: 0,
        options_count: OptionsCount::new(0, 0),
        service_id: request.service_id,
        instance_id: request.instance_id,
        major_version: request.major_version,
        ttl: request.ttl,
        counter: 0,
        event_group_id: request.event_group_id,
    });
    encode_sd_datagram(buf, &[entry], &[], session, RebootFlag::Continuous)
}

fn encode_service_entry_datagram(
    buf: &mut [u8],
    request: &OfferServiceRequest,
    session: u16,
    stop: bool,
) -> Result<usize, BuildError> {
    let svc = ServiceEntry {
        index_first_options_run: 0,
        index_second_options_run: 0,
        options_count: OptionsCount::new(1, 0),
        service_id: request.service_id,
        instance_id: request.instance_id,
        major_version: request.major_version,
        ttl: if stop { 0 } else { request.ttl },
        minor_version: request.minor_version,
    };
    let entry = if stop {
        Entry::StopOfferService(svc)
    } else {
        Entry::OfferService(svc)
    };
    let option = SdOptions::IpV4Endpoint {
        ip: request.local_ip,
        port: request.unicast_port,
        protocol: TransportProtocol::Udp,
    };
    encode_sd_datagram(buf, &[entry], &[option], session, RebootFlag::Continuous)
}

/// Encode `entries` + `options` as an SD payload, prefixed with the
/// SOME/IP wrapper built by [`Header::new_sd`]. Request ID is
/// `client_id=0 | session`.
fn encode_sd_datagram(
    buf: &mut [u8],
    entries: &[Entry],
    options: &[SdOptions],
    session: u16,
    reboot: RebootFlag,
) -> Result<usize, BuildError> {
    if buf.len() < SOMEIP_HEADER_LEN {
        return Err(BuildError::BufferTooSmall);
    }

    let sd_payload = SdHeader::new(Flags::new_sd(reboot), entries, options);
    let sd_payload_len = sd_payload
        .encode_to_slice(&mut buf[SOMEIP_HEADER_LEN..])
        .map_err(|_| BuildError::EncodeFailed)?;

    let header = Header::new_sd(u32::from(session), sd_payload_len);
    header
        .encode_to_slice(&mut buf[..SOMEIP_HEADER_LEN])
        .map_err(|_| BuildError::EncodeFailed)?;

    Ok(SOMEIP_HEADER_LEN + sd_payload_len)
}

/// Build a SOME/IP notification (event) datagram into `buf`: the
/// 16-byte SOME/IP header (message type `0x02` notification) followed by
/// `payload`. Returns the total length. Used by the firmware's publish
/// FFI so it never constructs the header itself.
///
/// # Errors
/// [`BuildError::BufferTooSmall`] if `buf` can't hold header + payload.
pub fn build_notification_datagram(
    buf: &mut [u8],
    service_id: u16,
    method_id: u16,
    session: u16,
    payload: &[u8],
) -> Result<usize, BuildError> {
    let total = SOMEIP_HEADER_LEN + payload.len();
    if buf.len() < total {
        return Err(BuildError::BufferTooSmall);
    }
    #[allow(clippy::cast_possible_truncation)]
    let length_field: u32 = 8 + payload.len() as u32;
    buf[0..2].copy_from_slice(&service_id.to_be_bytes());
    buf[2..4].copy_from_slice(&method_id.to_be_bytes());
    buf[4..8].copy_from_slice(&length_field.to_be_bytes());
    buf[8..10].copy_from_slice(&[0u8, 0u8]); // client-id
    buf[10..12].copy_from_slice(&session.to_be_bytes());
    buf[12] = 0x01; // protocol version
    buf[13] = 0x01; // interface version
    buf[14] = 0x02; // message type: notification
    buf[15] = 0x00; // return code: ok
    buf[SOMEIP_HEADER_LEN..total].copy_from_slice(payload);
    Ok(total)
}

/// Increment `counter` and return the next non-zero session ID. AUTOSAR
/// SD session IDs wrap `0xFFFF → 1`, skipping `0`.
pub fn next_sd_session(counter: &AtomicU16) -> u16 {
    loop {
        let s = counter.fetch_add(1, Ordering::Relaxed);
        if s != 0 {
            return s;
        }
    }
}

/// SOME/IP datagram fields extracted by [`parse_someip_datagram`];
/// `payload` borrows from the input buffer.
#[derive(Debug, Clone, Copy)]
pub struct ParsedDatagram<'a> {
    pub service_id: u16,
    pub method_id: u16,
    /// Header bytes 8..16. Consumed by E2E Profile 5 with-header CRC for
    /// protected messages.
    pub upper_header: [u8; 8],
    pub payload: &'a [u8],
}

/// Parse `data` as a SOME/IP datagram. Returns `None` if shorter than
/// [`SOMEIP_HEADER_LEN`] or [`HeaderView::parse`] rejects the header.
#[must_use]
pub fn parse_someip_datagram(data: &[u8]) -> Option<ParsedDatagram<'_>> {
    let (view, payload) = HeaderView::parse(data).ok()?;
    let message_id = view.message_id();
    Some(ParsedDatagram {
        service_id: message_id.service_id(),
        method_id: message_id.method_id(),
        upper_header: view.upper_header_bytes(),
        payload,
    })
}

/// Parse `data` as a SOME/IP-SD datagram, returning the inner
/// [`SdHeaderView`] for entry/option iteration. `None` if the wrapper
/// fails to parse, the message-ID is not SD, or the SD payload is bad.
#[must_use]
pub fn parse_someip_sd_datagram(data: &[u8]) -> Option<SdHeaderView<'_>> {
    let (view, sd_payload) = HeaderView::parse(data).ok()?;
    if !view.is_sd() {
        return None;
    }
    SdHeaderView::parse(sd_payload).ok()
}

/// Run an E2E check for `parsed` against `e2e`. Returns
/// `(Unchecked, parsed.payload)` when no profile is registered for the
/// `(service_id, method_id)` pair. Generic over [`E2ERegistryHandle`] so
/// it works with any handle (the bare-metal `StaticE2EHandle` included).
#[must_use]
pub fn check_parsed_e2e<'a, R: E2ERegistryHandle>(
    e2e: &R,
    parsed: &ParsedDatagram<'a>,
) -> (E2ECheckStatus, &'a [u8]) {
    let key = E2EKey::from_message_id(MessageId::new_from_service_and_method(
        parsed.service_id,
        parsed.method_id,
    ));
    match e2e.check(key, parsed.payload, parsed.upper_header) {
        Some((status, body)) => (status, body),
        None => (E2ECheckStatus::Unchecked, parsed.payload),
    }
}

/// Map an [`E2ECheckStatus`] to the 1-byte code the firmware dispatch
/// expects (`0` = unchecked / none). Shared so the library and firmware
/// agree on the wire mapping.
#[must_use]
pub fn e2e_status_code(status: E2ECheckStatus) -> u8 {
    match status {
        E2ECheckStatus::Ok => 1,
        E2ECheckStatus::CrcError => 2,
        E2ECheckStatus::Repeated => 3,
        E2ECheckStatus::OkSomeLost => 4,
        E2ECheckStatus::WrongSequence => 5,
        E2ECheckStatus::BadArgument => 6,
        E2ECheckStatus::Unchecked => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::sd::EntryType;

    fn req(service_id: u16, port: u16) -> OfferServiceRequest {
        OfferServiceRequest {
            service_id,
            instance_id: 1,
            major_version: 1,
            minor_version: 0,
            ttl: 3,
            local_ip: Ipv4Addr::new(192, 0, 2, 1),
            unicast_port: port,
        }
    }

    #[test]
    fn multi_offer_round_trips_through_sd_parser() {
        let offers = [req(0x0001, 30501), req(0x0002, 30502), req(0x0003, 30503)];
        let mut buf = [0u8; 512];
        let len = build_multi_offer_service_datagram::<8>(&mut buf, &offers, 7).unwrap();

        // Parses as an SD datagram with one OfferService entry per offer,
        // each referencing its own endpoint option.
        let view = parse_someip_sd_datagram(&buf[..len]).expect("valid SD datagram");
        let services: heapless::Vec<u16, 8> = view
            .entries()
            .filter_map(|e| {
                (e.entry_type().ok()? == EntryType::OfferService).then_some(e.service_id())
            })
            .collect();
        assert_eq!(services.as_slice(), &[0x0001, 0x0002, 0x0003]);
    }

    #[test]
    fn build_notification_round_trips_through_someip_parser() {
        let payload = [0xDE, 0xAD, 0xBE, 0xEF];
        let mut buf = [0u8; 64];
        let len = build_notification_datagram(&mut buf, 0x0003, 0x8001, 9, &payload).unwrap();
        let parsed = parse_someip_datagram(&buf[..len]).expect("valid SOME/IP datagram");
        assert_eq!(parsed.service_id, 0x0003);
        assert_eq!(parsed.method_id, 0x8001);
        assert_eq!(parsed.payload, &payload);
    }

    #[test]
    fn subscribe_builder_honors_reboot_flag() {
        let request = SubscribeEventgroupRequest {
            service_id: 0x0042,
            instance_id: 1,
            major_version: 1,
            event_group_id: 1,
            ttl: 0x00FF_FFFF,
            local_ip: Ipv4Addr::new(192, 0, 2, 2),
            local_rx_port: 30600,
        };
        let mut buf = [0u8; 128];
        // Both reboot flags must encode to a parseable subscribe datagram;
        // the flag lives in the SD header flags byte (bit 7).
        for reboot in [RebootFlag::RecentlyRebooted, RebootFlag::Continuous] {
            let len = build_subscribe_eventgroup_datagram(&mut buf, &request, 3, reboot).unwrap();
            let view = parse_someip_sd_datagram(&buf[..len]).expect("valid SD datagram");
            let mut entries = view.entries();
            let entry = entries.next().expect("one entry");
            assert_eq!(entry.entry_type().unwrap(), EntryType::Subscribe);
            assert_eq!(entry.service_id(), 0x0042);
        }
    }
}
