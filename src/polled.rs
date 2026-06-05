//! Synchronous polled SOME/IP helpers for bare-metal targets.
//!
//! Gated by `feature = "bare_metal_poll"`. Transport-agnostic SD
//! packet builders + SOMEIP datagram parsers driven from a caller's
//! periodic tick instead of the async `Client`/`Server` paths.
//!
//! See `docs/polled-bare-metal-rationale.md` for the memory-cost
//! comparison that motivates this module.

use core::net::Ipv4Addr;
use core::sync::atomic::{AtomicU16, Ordering};

use crate::E2ECheckStatus;
use crate::E2EKey;
use crate::StaticE2EHandle;
use crate::WireFormat;
use crate::protocol::{HEADER_SIZE, Header, HeaderView, MessageId};
use crate::protocol::sd::{
    Entry, EventGroupEntry, Flags, Header as SdHeader, Options as SdOptions, OptionsCount,
    RebootFlag, SdHeaderView, ServiceEntry, TransportProtocol,
};
use crate::transport::E2ERegistryHandle;

/// SOME/IP header size, re-exported so callers can name the
/// payload offset returned by [`parse_someip_datagram`].
pub use crate::protocol::HEADER_SIZE as SOMEIP_HEADER_LEN;

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
/// length in bytes.
///
/// `session` should come from [`next_sd_session`].
///
/// # Errors
/// See [`BuildError`].
pub fn build_subscribe_eventgroup_datagram(
    buf: &mut [u8],
    request: &SubscribeEventgroupRequest,
    session: u16,
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
    encode_sd_datagram(buf, &[entry], &[option], session)
}

/// Encode an `OfferService` datagram into `buf`. Returns its
/// length in bytes.
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
    encode_sd_datagram(buf, &[entry], &[], session)
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
    encode_sd_datagram(buf, &[entry], &[option], session)
}

/// Encode `entries` + `options` as an SD payload, prefixed with the
/// SOME/IP wrapper built by [`Header::new_sd`] (which owns every
/// wire-format constant). Request ID is `client_id=0 | session`.
fn encode_sd_datagram(
    buf: &mut [u8],
    entries: &[Entry],
    options: &[SdOptions],
    session: u16,
) -> Result<usize, BuildError> {
    if buf.len() < HEADER_SIZE {
        return Err(BuildError::BufferTooSmall);
    }

    let sd_payload = SdHeader::new(Flags::new_sd(RebootFlag::Continuous), entries, options);
    let sd_payload_len = sd_payload
        .encode_to_slice(&mut buf[HEADER_SIZE..])
        .map_err(|_| BuildError::EncodeFailed)?;

    let header = Header::new_sd(u32::from(session), sd_payload_len);
    header
        .encode_to_slice(&mut buf[..HEADER_SIZE])
        .map_err(|_| BuildError::EncodeFailed)?;

    Ok(HEADER_SIZE + sd_payload_len)
}

/// Increment `counter` and return the next non-zero session ID.
///
/// AUTOSAR SD session IDs wrap `0xFFFF → 1`, skipping `0`.
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
    /// Header bytes 8..16. Consumed by E2E Profile 5 with-header
    /// CRC for protected messages.
    pub upper_header: [u8; 8],
    pub payload: &'a [u8],
}

/// Parse `data` as a SOME/IP datagram. Returns `None` if shorter
/// than [`SOMEIP_HEADER_LEN`] or [`HeaderView::parse`] rejects the
/// header (bad protocol version, message type, or return code).
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
/// [`SdHeaderView`] for entry/option iteration.
///
/// Returns `None` if the SOME/IP wrapper fails to parse, the
/// message-ID is not [`MessageId::SD`], or the SD payload fails to
/// parse.
#[must_use]
pub fn parse_someip_sd_datagram(data: &[u8]) -> Option<SdHeaderView<'_>> {
    let (view, sd_payload) = HeaderView::parse(data).ok()?;
    if !view.is_sd() {
        return None;
    }
    SdHeaderView::parse(sd_payload).ok()
}

/// Run E2E check for `parsed` against `e2e`. Returns
/// `(Unchecked, parsed.payload)` when no profile is registered for
/// the `(service_id, method_id)` pair.
#[must_use]
pub fn check_parsed_e2e<'a>(
    e2e: &StaticE2EHandle,
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
