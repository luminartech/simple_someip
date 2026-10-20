//! Pure, no-alloc SOME/IP + SD datagram codec for bare-metal targets.
//!
//! Transport-agnostic builders and parsers: encode `OfferService` /
//! `StopOfferService` / `SubscribeEventgroup` / `SubscribeAckEventgroup`
//! SD datagrams, and parse inbound SOME/IP / SOME/IP-SD datagrams. No
//! async, no sockets, no allocation — callers own the scratch buffer and
//! the transmit path. These back the spawnable futures in
//! [`crate::bare_metal_tasks`] and the firmware's publish/deinit FFI, so
//! that no SOME/IP byte-encoding or header-parsing lives in the firmware.

use core::net::{IpAddr, Ipv4Addr};
use core::sync::atomic::{AtomicU16, Ordering};

use automotive_wire_codec::{EncodeToSliceError, InsufficientBuffer};

use crate::Encode;
use crate::protocol::sd::{
    Entry, EventGroupEntry, Flags, Header as SdHeader, Options as SdOptions, OptionsCount,
    RebootFlag, SdHeaderView, ServiceEntry, TransportProtocol,
};
use crate::protocol::{Header, HeaderView, MessageId, MessageType, MessageTypeField, ReturnCode};
use crate::transport::{E2EHandle, E2EStatus};

/// SOME/IP header length in bytes — the offset at which an SD or
/// notification payload begins.
pub const SOMEIP_HEADER_LEN: usize = 16;

/// Whether a Subscribe for these IDs names a wildcard service ID (`0xFFFF`),
/// instance ID (`0xFFFF`) or major version (`0xFF`). A Subscribe names the
/// service, instance and version it subscribes to.
#[cfg(feature = "server")]
pub(crate) const fn names_a_wildcard(service_id: u16, instance_id: u16, major_version: u8) -> bool {
    service_id == 0xFFFF || instance_id == 0xFFFF || major_version == 0xFF
}

/// One `SubscribeEventgroup` entry, with the local endpoint the
/// publisher should deliver events to.
#[derive(Clone, Copy, Debug)]
pub struct SubscribeEventgroupRequest {
    pub service_id: u16,
    pub instance_id: u16,
    pub major_version: u8,
    pub event_group_id: u16,
    /// Subscription TTL in seconds, at most
    /// [`MAX_TTL`](crate::protocol::sd::MAX_TTL).
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
    /// Offer TTL in seconds, 1 to [`MAX_TTL`](crate::protocol::sd::MAX_TTL).
    /// [`build_stop_offer_service_datagram`] sends 0 instead.
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
    /// The counter of the `Subscribe` being answered.
    pub counter: u16,
}

/// Packet-construction errors.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[non_exhaustive]
pub enum BuildError {
    /// `buf` is shorter than the encoded datagram. Carries the codec's
    /// [`InsufficientBuffer`] so callers see the needed / available byte
    /// counts, matching the rest of the crate's buffer-too-small reporting.
    BufferTooSmall(InsufficientBuffer),
    /// A fixed-capacity entry or option list could not take another element.
    ///
    /// Distinct from [`BufferTooSmall`](Self::BufferTooSmall), whose
    /// [`InsufficientBuffer`] fields are documented in *bytes*. Element counts
    /// were previously reported through that variant, which read as a byte
    /// count and would have been actively misleading if it ever fired.
    ListFull {
        /// Elements the caller tried to place.
        needed: usize,
        /// Elements the fixed-capacity list can hold.
        capacity: usize,
    },
    /// An entry's TTL is above [`MAX_TTL`](crate::protocol::sd::MAX_TTL), or
    /// an offer's TTL is 0; carries the rejected TTL.
    InvalidTtl(u32),
    /// Sizing or encoding the SD or SOME/IP datagram failed for a reason
    /// other than [`BuildError::InvalidTtl`].
    EncodeFailed,
}

impl core::fmt::Display for BuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BuildError::BufferTooSmall(ib) => write!(f, "buffer too small: {ib}"),
            BuildError::ListFull { needed, capacity } => write!(
                f,
                "fixed-capacity list full: needed {needed} elements, capacity {capacity}"
            ),
            BuildError::InvalidTtl(ttl) => {
                write!(
                    f,
                    "SD entry TTL {ttl} is invalid: an offer needs 1..=0xFFFFFF seconds, other entries at most 0xFFFFFF"
                )
            }
            BuildError::EncodeFailed => f.write_str("sizing or encoding failed"),
        }
    }
}

impl core::error::Error for BuildError {}

impl From<InsufficientBuffer> for BuildError {
    fn from(ib: InsufficientBuffer) -> Self {
        BuildError::BufferTooSmall(ib)
    }
}

impl From<EncodeToSliceError<crate::protocol::Error>> for BuildError {
    fn from(e: EncodeToSliceError<crate::protocol::Error>) -> Self {
        match e {
            EncodeToSliceError::InsufficientBuffer(ib) => BuildError::BufferTooSmall(ib),
            EncodeToSliceError::Encode(e) => encode_error(&e),
        }
    }
}

fn encode_error(e: &crate::protocol::Error) -> BuildError {
    match e {
        crate::protocol::Error::Sd(crate::protocol::sd::Error::InvalidTtl(ttl)) => {
            BuildError::InvalidTtl(*ttl)
        }
        _ => BuildError::EncodeFailed,
    }
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
    build_multi_subscribe_eventgroup_datagram::<1>(
        buf,
        core::slice::from_ref(request),
        session,
        reboot,
    )
}

/// Length of the SD datagram [`build_multi_subscribe_eventgroup_datagram`]
/// encodes for `requests` Subscribe entries.
#[cfg(feature = "server")]
pub(crate) const fn multi_subscribe_datagram_len(requests: usize) -> usize {
    // SD header: flags and reserved bytes, entries length, options length.
    const SD_HEADER_LEN: usize = 12;
    SOMEIP_HEADER_LEN
        + SD_HEADER_LEN
        + requests * (crate::protocol::sd::ENTRY_SIZE + crate::protocol::sd::IPV4_OPTION_WIRE_SIZE)
}

/// Encode a single SD datagram carrying one `SubscribeEventgroup` entry per
/// element of `requests`, each referencing its own IPv4 endpoint option.
/// `reboot` sets the SD reboot flag, as in
/// [`build_subscribe_eventgroup_datagram`].
///
/// # Errors
/// Returns [`BuildError::ListFull`] if `requests` holds more than `N`
/// elements; otherwise see [`BuildError`].
pub fn build_multi_subscribe_eventgroup_datagram<const N: usize>(
    buf: &mut [u8],
    requests: &[SubscribeEventgroupRequest],
    session: u16,
    reboot: RebootFlag,
) -> Result<usize, BuildError> {
    const { assert!(N <= 256, "SD option indices are one byte") };
    if requests.len() > N {
        return Err(BuildError::ListFull {
            needed: requests.len(),
            capacity: N,
        });
    }
    let mut entries: heapless::Vec<Entry, N> = heapless::Vec::new();
    let mut options: heapless::Vec<SdOptions, N> = heapless::Vec::new();
    for (index, request) in (0..=u8::MAX).zip(requests) {
        let _ = entries.push(Entry::SubscribeEventGroup(EventGroupEntry {
            index_first_options_run: index,
            index_second_options_run: 0,
            options_count: OptionsCount::new(1, 0),
            service_id: request.service_id,
            instance_id: request.instance_id,
            major_version: request.major_version,
            ttl: request.ttl,
            counter: 0,
            event_group_id: request.event_group_id,
        }));
        let _ = options.push(SdOptions::IpV4Endpoint {
            ip: request.local_ip,
            port: request.local_rx_port,
            protocol: TransportProtocol::Udp,
        });
    }
    encode_sd_datagram(buf, &entries, &options, session, reboot)
}

/// Encode an `OfferService` datagram into `buf`. Returns its length in
/// bytes. `reboot` sets the SD reboot flag: pass
/// [`RebootFlag::RecentlyRebooted`] until the counter `session` comes from
/// first wraps from 0xFFFF to 1, and [`RebootFlag::Continuous`] after.
/// Multicast and each unicast peer have a counter of their own.
///
/// # Errors
/// See [`BuildError`].
pub fn build_offer_service_datagram(
    buf: &mut [u8],
    request: &OfferServiceRequest,
    session: u16,
    reboot: RebootFlag,
) -> Result<usize, BuildError> {
    encode_service_entry_datagram(buf, request, session, /*stop=*/ false, reboot)
}

/// Encode a `StopOfferService` datagram into `buf` (same shape as
/// [`build_offer_service_datagram`], TTL forced to `0`). `reboot` sets the
/// SD reboot flag, as in [`build_offer_service_datagram`].
///
/// # Errors
/// See [`BuildError`].
pub fn build_stop_offer_service_datagram(
    buf: &mut [u8],
    request: &OfferServiceRequest,
    session: u16,
    reboot: RebootFlag,
) -> Result<usize, BuildError> {
    encode_service_entry_datagram(buf, request, session, /*stop=*/ true, reboot)
}

/// Encode a single SD datagram carrying one `OfferService` entry per
/// element of `requests` (up to `N`). Each entry references its own
/// IPv4 endpoint option at the matching flat index — one coherent SD
/// message instead of one packet per service. `reboot` sets the SD reboot
/// flag, as in [`build_offer_service_datagram`].
///
/// # Errors
/// See [`BuildError`].
pub fn build_multi_offer_service_datagram<const N: usize>(
    buf: &mut [u8],
    requests: &[OfferServiceRequest],
    session: u16,
    reboot: RebootFlag,
) -> Result<usize, BuildError> {
    build_multi_service_entry_datagram::<N>(buf, requests, session, /*stop=*/ false, reboot)
}

/// Encode a single SD datagram carrying one `StopOfferService` entry per
/// element of `requests` (up to `N`). `reboot` sets the SD reboot flag, as
/// in [`build_offer_service_datagram`].
///
/// # Errors
/// See [`build_multi_offer_service_datagram`].
pub fn build_multi_stop_offer_service_datagram<const N: usize>(
    buf: &mut [u8],
    requests: &[OfferServiceRequest],
    session: u16,
    reboot: RebootFlag,
) -> Result<usize, BuildError> {
    build_multi_service_entry_datagram::<N>(buf, requests, session, /*stop=*/ true, reboot)
}

fn build_multi_service_entry_datagram<const N: usize>(
    buf: &mut [u8],
    requests: &[OfferServiceRequest],
    session: u16,
    stop: bool,
    reboot: RebootFlag,
) -> Result<usize, BuildError> {
    const { assert!(N <= 256, "SD option indices are one byte") };
    if requests.len() > N {
        return Err(BuildError::ListFull {
            needed: requests.len(),
            capacity: N,
        });
    }
    let mut entries: heapless::Vec<Entry, N> = heapless::Vec::new();
    let mut options: heapless::Vec<SdOptions, N> = heapless::Vec::new();
    for (index, req) in (0..=u8::MAX).zip(requests) {
        let svc = ServiceEntry {
            index_first_options_run: index,
            index_second_options_run: 0,
            options_count: OptionsCount::new(1, 0),
            service_id: req.service_id,
            instance_id: req.instance_id,
            major_version: req.major_version,
            ttl: req.ttl,
            minor_version: req.minor_version,
        };
        let _ = entries.push(if stop {
            Entry::StopOfferService(svc)
        } else {
            Entry::OfferService(svc)
        });
        let _ = options.push(SdOptions::IpV4Endpoint {
            ip: req.local_ip,
            port: req.unicast_port,
            protocol: TransportProtocol::Udp,
        });
    }
    encode_sd_datagram(buf, &entries, &options, session, reboot)
}

/// Encode one SD datagram with a `FindService` entry, any minor version and
/// no options, for each element of `wanted`.
#[cfg(feature = "server")]
pub(crate) fn build_find_service_datagram<const N: usize>(
    buf: &mut [u8],
    wanted: &[SubscribeEventgroupRequest],
    session: u16,
    reboot: RebootFlag,
) -> Result<usize, BuildError> {
    let mut entries: heapless::Vec<Entry, N> = heapless::Vec::new();
    for request in wanted {
        entries
            .push(Entry::FindService(ServiceEntry {
                index_first_options_run: 0,
                index_second_options_run: 0,
                options_count: OptionsCount::new(0, 0),
                service_id: request.service_id,
                instance_id: request.instance_id,
                major_version: request.major_version,
                ttl: request.ttl,
                minor_version: 0xFFFF_FFFF,
            }))
            .map_err(|_| BuildError::ListFull {
                needed: wanted.len(),
                capacity: N,
            })?;
    }
    encode_sd_datagram(buf, &entries, &[], session, reboot)
}

/// Encode a `SubscribeAckEventgroup` datagram into `buf`. Caller
/// transmits to the original `Subscribe` sender's SD endpoint. `reboot`
/// sets the SD reboot flag of that peer's unicast session counter, as
/// described for [`build_offer_service_datagram`].
///
/// # Errors
/// See [`BuildError`].
pub fn build_subscribe_ack_datagram(
    buf: &mut [u8],
    request: &SubscribeAckRequest,
    session: u16,
    reboot: RebootFlag,
) -> Result<usize, BuildError> {
    let entry = Entry::SubscribeAckEventGroup(EventGroupEntry {
        index_first_options_run: 0,
        index_second_options_run: 0,
        options_count: OptionsCount::new(0, 0),
        service_id: request.service_id,
        instance_id: request.instance_id,
        major_version: request.major_version,
        ttl: request.ttl,
        counter: request.counter,
        event_group_id: request.event_group_id,
    });
    encode_sd_datagram(buf, &[entry], &[], session, reboot)
}

fn encode_service_entry_datagram(
    buf: &mut [u8],
    request: &OfferServiceRequest,
    session: u16,
    stop: bool,
    reboot: RebootFlag,
) -> Result<usize, BuildError> {
    let svc = ServiceEntry {
        index_first_options_run: 0,
        index_second_options_run: 0,
        options_count: OptionsCount::new(1, 0),
        service_id: request.service_id,
        instance_id: request.instance_id,
        major_version: request.major_version,
        ttl: request.ttl,
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
    encode_sd_datagram(buf, &[entry], &[option], session, reboot)
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
    // Header-first, body-second: `sd::Header::encoded_size()` is exact and
    // computable without writing, so the SOME/IP length field is known before
    // either section is written. One linear forward pass — no backfill.
    let sd_payload = SdHeader::new(Flags::new_sd(reboot), entries, options);
    let sd_payload_len = sd_payload.encoded_size().map_err(|e| encode_error(&e))?;
    let total = SOMEIP_HEADER_LEN + sd_payload_len;
    if buf.len() < total {
        return Err(BuildError::BufferTooSmall(InsufficientBuffer {
            needed: total,
            available: buf.len(),
        }));
    }

    let header = Header::new_sd(u32::from(session), sd_payload_len);
    header.encode_to_slice(&mut buf[..SOMEIP_HEADER_LEN])?;
    let written = sd_payload.encode_to_slice(&mut buf[SOMEIP_HEADER_LEN..total])?;

    Ok(SOMEIP_HEADER_LEN + written)
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
        return Err(BuildError::BufferTooSmall(InsufficientBuffer {
            needed: total,
            available: buf.len(),
        }));
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

/// Write the RESPONSE header for a reply into `buf[..SOMEIP_HEADER_LEN]`:
/// echoes the request's message/request id and protocol/interface versions,
/// with message type RESPONSE and return code OK. Only the header is written
/// — the `payload_len`-byte payload is assumed already in place after it.
///
/// # Errors
/// [`BuildError::BufferTooSmall`] if `buf` is shorter than the header;
/// [`BuildError::EncodeFailed`] if header encoding fails.
pub fn encode_response_header(
    buf: &mut [u8],
    service_id: u16,
    method_id: u16,
    request_id: u32,
    protocol_version: u8,
    interface_version: u8,
    payload_len: usize,
) -> Result<(), BuildError> {
    if buf.len() < SOMEIP_HEADER_LEN {
        return Err(BuildError::BufferTooSmall(InsufficientBuffer {
            needed: SOMEIP_HEADER_LEN,
            available: buf.len(),
        }));
    }
    let header = Header::new(
        MessageId::new_from_service_and_method(service_id, method_id),
        request_id,
        protocol_version,
        interface_version,
        MessageTypeField::new(MessageType::Response, false),
        ReturnCode::Ok,
        payload_len,
    );
    header
        .encode_to_slice(&mut buf[..SOMEIP_HEADER_LEN])
        .map_err(|_| BuildError::EncodeFailed)?;
    Ok(())
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
    /// Header bytes 8..16, passed to the
    /// [`E2EHandle`] check.
    pub upper_header: [u8; 8],
    pub payload: &'a [u8],
}

/// Parse `data` as a SOME/IP datagram.
///
/// # Errors
/// Propagates the [`HeaderView::parse`] error: [`protocol::Error::Incomplete`]
/// if `data` is shorter than a header can consume, or a validation error if the
/// header fields are malformed. This is the same error type used throughout the
/// crate, so callers can distinguish "not enough bytes yet" from "malformed".
///
/// [`protocol::Error::Incomplete`]: crate::protocol::Error::Incomplete
pub fn parse_someip_datagram(data: &[u8]) -> Result<ParsedDatagram<'_>, crate::protocol::Error> {
    let (view, payload) = HeaderView::parse(data)?;
    let message_id = view.message_id();
    Ok(ParsedDatagram {
        service_id: message_id.service_id(),
        method_id: message_id.method_id(),
        upper_header: view.upper_header_bytes(),
        payload,
    })
}

/// Parse `data` as a SOME/IP-SD datagram, returning the inner
/// [`SdHeaderView`] for entry/option iteration.
///
/// # Errors
/// The three failure modes are distinguishable by variant:
/// - [`protocol::Error::Incomplete`] — the SOME/IP wrapper needs more bytes.
/// - [`protocol::Error::UnsupportedMessageID`] — a well-formed SOME/IP message
///   whose message-ID is not [`MessageId::SD`] (i.e. not an SD message).
/// - [`protocol::Error::Sd`] (and other validation variants) — the wrapper or
///   the inner SD payload is malformed.
///
/// [`protocol::Error::Incomplete`]: crate::protocol::Error::Incomplete
/// [`protocol::Error::UnsupportedMessageID`]: crate::protocol::Error::UnsupportedMessageID
/// [`protocol::Error::Sd`]: crate::protocol::Error::Sd
pub fn parse_someip_sd_datagram(data: &[u8]) -> Result<SdHeaderView<'_>, crate::protocol::Error> {
    let (view, sd_payload) = HeaderView::parse(data)?;
    if !view.is_sd() {
        return Err(crate::protocol::Error::UnsupportedMessageID(
            view.message_id(),
        ));
    }
    SdHeaderView::parse(sd_payload)
}

/// Runs `e2e`'s check on `parsed`, received from `source`. Returns what the
/// check found, or `None` for an SD message or when no check applies, and
/// the payload to decode.
#[must_use]
pub fn check_parsed_e2e<'a, R: E2EHandle + ?Sized>(
    e2e: &R,
    source: IpAddr,
    parsed: &ParsedDatagram<'a>,
) -> (Option<E2EStatus>, &'a [u8]) {
    crate::transport::check_message(
        e2e,
        source,
        MessageId::new_from_service_and_method(parsed.service_id, parsed.method_id),
        parsed.upper_header,
        parsed.payload,
    )
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
    fn offer_with_zero_ttl_is_rejected() {
        let mut buf = [0u8; 512];
        let request = OfferServiceRequest {
            ttl: 0,
            ..req(0x0001, 30501)
        };
        assert_eq!(
            build_offer_service_datagram(&mut buf, &request, 1, RebootFlag::Continuous),
            Err(BuildError::InvalidTtl(0))
        );
        assert_eq!(
            build_multi_offer_service_datagram::<4>(
                &mut buf,
                &[request],
                1,
                RebootFlag::Continuous
            ),
            Err(BuildError::InvalidTtl(0))
        );
    }

    #[test]
    fn stop_offer_is_built_whatever_the_requested_ttl() {
        let mut buf = [0u8; 512];
        for ttl in [3, u32::MAX] {
            let request = OfferServiceRequest {
                ttl,
                ..req(0x0001, 30501)
            };
            let len =
                build_stop_offer_service_datagram(&mut buf, &request, 1, RebootFlag::Continuous)
                    .unwrap();
            let view = parse_someip_sd_datagram(&buf[..len]).expect("valid SD datagram");
            let entry = view.entries().next().unwrap();
            assert!(entry.is_stop_offer());
        }
    }

    #[test]
    fn multi_offer_round_trips_through_sd_parser() {
        let offers = [req(0x0001, 30501), req(0x0002, 30502), req(0x0003, 30503)];
        let mut buf = [0u8; 512];
        let len =
            build_multi_offer_service_datagram::<8>(&mut buf, &offers, 7, RebootFlag::Continuous)
                .unwrap();

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
    fn build_too_small_buffer_reports_needed_and_available() {
        let request = SubscribeEventgroupRequest {
            service_id: 0x0042,
            instance_id: 1,
            major_version: 1,
            event_group_id: 1,
            ttl: 3,
            local_ip: Ipv4Addr::new(192, 0, 2, 2),
            local_rx_port: 30600,
        };
        // Enough for the SOME/IP header but not the SD body.
        let mut buf = [0u8; SOMEIP_HEADER_LEN];
        let err =
            build_subscribe_eventgroup_datagram(&mut buf, &request, 3, RebootFlag::Continuous)
                .unwrap_err();
        match err {
            BuildError::BufferTooSmall(ib) => {
                assert_eq!(ib.available, SOMEIP_HEADER_LEN);
                assert!(ib.needed > ib.available);
            }
            other => panic!("expected BufferTooSmall, got {other:?}"),
        }
    }

    #[test]
    fn parse_someip_datagram_incomplete_is_error() {
        // Fewer than SOMEIP_HEADER_LEN bytes -> Incomplete, not a silent None.
        let err = parse_someip_datagram(&[0u8; 4]).unwrap_err();
        assert!(matches!(err, crate::protocol::Error::Incomplete(_)));
    }

    #[test]
    fn parse_sd_datagram_incomplete_vs_not_sd_vs_malformed() {
        // 1. Incomplete: too few bytes for even the SOME/IP header.
        let err = parse_someip_sd_datagram(&[0u8; 4]).unwrap_err();
        assert!(matches!(err, crate::protocol::Error::Incomplete(_)));

        // 2. Not SD: a well-formed non-SD notification datagram.
        let mut buf = [0u8; 64];
        let len = build_notification_datagram(&mut buf, 0x0003, 0x8001, 9, &[1, 2, 3]).unwrap();
        let err = parse_someip_sd_datagram(&buf[..len]).unwrap_err();
        assert!(matches!(
            err,
            crate::protocol::Error::UnsupportedMessageID(_)
        ));

        // 3. Truncated SD datagram: the SD message-ID is present but the
        //    payload is chopped short. This is an error, and crucially it is
        //    NOT reported as the not-SD (`UnsupportedMessageID`) case — the
        //    three failure modes stay distinguishable.
        let request = SubscribeEventgroupRequest {
            service_id: 0x0042,
            instance_id: 1,
            major_version: 1,
            event_group_id: 1,
            ttl: 3,
            local_ip: Ipv4Addr::new(192, 0, 2, 2),
            local_rx_port: 30600,
        };
        let mut sd_buf = [0u8; 128];
        let sd_len =
            build_subscribe_eventgroup_datagram(&mut sd_buf, &request, 3, RebootFlag::Continuous)
                .unwrap();
        let truncated = &sd_buf[..sd_len - 4];
        let err = parse_someip_sd_datagram(truncated).unwrap_err();
        assert!(
            !matches!(err, crate::protocol::Error::UnsupportedMessageID(_)),
            "truncated SD datagram must not surface as not-SD"
        );
    }

    #[test]
    fn parse_sd_datagram_malformed_known_option_is_sd_error() {
        let mut buf = [0u8; 128];
        let len =
            build_offer_service_datagram(&mut buf, &req(0x0001, 30501), 1, RebootFlag::Continuous)
                .unwrap();
        // The IPv4 endpoint option is the last 12 bytes; its transport byte is
        // the third-to-last.
        buf[len - 3] = 0x84;
        let err = parse_someip_sd_datagram(&buf[..len]).unwrap_err();
        assert!(
            matches!(
                err,
                crate::protocol::Error::Sd(
                    crate::protocol::sd::Error::InvalidOptionTransportProtocol(0x84)
                )
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn parse_sd_datagram_skips_an_entry_of_unknown_type() {
        let request = SubscribeEventgroupRequest {
            service_id: 0x0042,
            instance_id: 1,
            major_version: 1,
            event_group_id: 1,
            ttl: 3,
            local_ip: Ipv4Addr::new(192, 0, 2, 2),
            local_rx_port: 30600,
        };
        let mut sd_buf = [0u8; 128];
        let sd_len =
            build_subscribe_eventgroup_datagram(&mut sd_buf, &request, 3, RebootFlag::Continuous)
                .unwrap();

        // The first entry's type byte follows the 16-byte SOME/IP header and
        // the SD flags/reserved and entries_size fields.
        sd_buf[16 + 8] = 0xFF;

        let view =
            parse_someip_sd_datagram(&sd_buf[..sd_len]).expect("an unknown entry is skipped");
        assert_eq!(view.entries().count(), 0);
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
            assert_eq!(view.flags().reboot(), reboot);
        }
    }

    #[test]
    fn multi_subscribe_entries_each_reference_their_own_endpoint() {
        let request = |event_group_id, local_rx_port| SubscribeEventgroupRequest {
            service_id: 0x0042,
            instance_id: 1,
            major_version: 1,
            event_group_id,
            ttl: 5,
            local_ip: Ipv4Addr::new(192, 0, 2, 2),
            local_rx_port,
        };
        let requests = [request(1, 30600), request(2, 30601)];
        let mut buf = [0u8; 256];
        let len = build_multi_subscribe_eventgroup_datagram::<2>(
            &mut buf,
            &requests,
            3,
            RebootFlag::Continuous,
        )
        .unwrap();
        let view = parse_someip_sd_datagram(&buf[..len]).expect("valid SD datagram");
        let endpoints: std::vec::Vec<_> = view
            .entries()
            .map(|e| {
                assert_eq!(e.entry_type().unwrap(), EntryType::Subscribe);
                let (_, _, port) = view
                    .options()
                    .nth(usize::from(e.index_first_options_run()))
                    .expect("the referenced option exists")
                    .as_ipv4()
                    .expect("an IPv4 endpoint");
                (e.event_group_id(), port)
            })
            .collect();
        assert_eq!(endpoints.len(), 2);
        let expected: std::vec::Vec<_> = requests
            .iter()
            .map(|r| (r.event_group_id, r.local_rx_port))
            .collect();
        assert_eq!(endpoints, expected);
    }

    #[cfg(feature = "server")]
    #[test]
    fn multi_subscribe_datagram_len_is_the_encoded_length() {
        let request = SubscribeEventgroupRequest {
            service_id: 0x0042,
            instance_id: 1,
            major_version: 1,
            event_group_id: 1,
            ttl: 5,
            local_ip: Ipv4Addr::new(192, 0, 2, 2),
            local_rx_port: 30600,
        };
        let mut buf = [0u8; 256];
        for n in 1..=3 {
            let requests = [request; 3];
            let len = build_multi_subscribe_eventgroup_datagram::<3>(
                &mut buf,
                &requests[..n],
                3,
                RebootFlag::Continuous,
            )
            .unwrap();
            assert_eq!(multi_subscribe_datagram_len(n), len, "{n} requests");
        }
    }

    #[test]
    fn multi_subscribe_rejects_more_requests_than_its_capacity() {
        let request = SubscribeEventgroupRequest {
            service_id: 0x0042,
            instance_id: 1,
            major_version: 1,
            event_group_id: 1,
            ttl: 5,
            local_ip: Ipv4Addr::new(192, 0, 2, 2),
            local_rx_port: 30600,
        };
        let mut buf = [0u8; 256];
        assert_eq!(
            build_multi_subscribe_eventgroup_datagram::<1>(
                &mut buf,
                &[request, request],
                3,
                RebootFlag::Continuous
            ),
            Err(BuildError::ListFull {
                needed: 2,
                capacity: 1
            })
        );
    }

    #[cfg(feature = "server")]
    fn find_request() -> SubscribeEventgroupRequest {
        SubscribeEventgroupRequest {
            service_id: 0x0042,
            instance_id: 3,
            major_version: 2,
            event_group_id: 1,
            ttl: 7,
            local_ip: Ipv4Addr::new(192, 0, 2, 2),
            local_rx_port: 30600,
        }
    }

    #[cfg(feature = "server")]
    #[test]
    fn find_entries_ask_for_any_minor_version_and_carry_no_options() {
        let mut buf = [0u8; 64];
        let len = build_find_service_datagram::<1>(
            &mut buf,
            &[find_request()],
            5,
            RebootFlag::Continuous,
        )
        .unwrap();
        let view = parse_someip_sd_datagram(&buf[..len]).expect("valid SD datagram");
        let entries: std::vec::Vec<_> = view.entries().collect();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.entry_type().unwrap(), EntryType::FindService);
        assert_eq!(
            (e.service_id(), e.instance_id(), e.major_version(), e.ttl()),
            (0x0042, 3, 2, 7)
        );
        assert_eq!(e.minor_version(), 0xFFFF_FFFF);
        assert_eq!(view.options().count(), 0);
        assert_eq!(len, 16 + 12 + 16);
    }

    #[cfg(feature = "server")]
    #[test]
    fn find_rejects_more_instances_than_its_capacity() {
        let mut buf = [0u8; 64];
        assert_eq!(
            build_find_service_datagram::<1>(
                &mut buf,
                &[find_request(), find_request()],
                5,
                RebootFlag::Continuous
            ),
            Err(BuildError::ListFull {
                needed: 2,
                capacity: 1
            })
        );
    }

    #[test]
    fn offer_builders_write_the_reboot_flag_they_are_given() {
        type Builder = fn(&mut [u8], &OfferServiceRequest, RebootFlag) -> Result<usize, BuildError>;
        let builders: [(&str, Builder); 4] = [
            ("offer", |b, r, f| build_offer_service_datagram(b, r, 1, f)),
            ("stop", |b, r, f| {
                build_stop_offer_service_datagram(b, r, 1, f)
            }),
            ("multi offer", |b, r, f| {
                build_multi_offer_service_datagram::<1>(b, core::slice::from_ref(r), 1, f)
            }),
            ("multi stop", |b, r, f| {
                build_multi_stop_offer_service_datagram::<1>(b, core::slice::from_ref(r), 1, f)
            }),
        ];
        for (name, build) in builders {
            for reboot in [RebootFlag::RecentlyRebooted, RebootFlag::Continuous] {
                let mut buf = [0u8; 128];
                let len = build(&mut buf, &req(0x0001, 30501), reboot).unwrap();
                let view = parse_someip_sd_datagram(&buf[..len]).expect("valid SD datagram");
                assert_eq!(view.flags().reboot(), reboot, "{name}");
            }
        }
    }

    #[test]
    fn multi_offer_builders_reject_more_requests_than_their_capacity() {
        let mut buf = [0u8; 256];
        let requests = [req(0x0001, 30501), req(0x0002, 30502)];
        let full = Err(BuildError::ListFull {
            needed: 2,
            capacity: 1,
        });
        assert_eq!(
            build_multi_offer_service_datagram::<1>(&mut buf, &requests, 1, RebootFlag::Continuous),
            full
        );
        assert_eq!(
            build_multi_stop_offer_service_datagram::<1>(
                &mut buf,
                &requests,
                1,
                RebootFlag::Continuous
            ),
            full
        );
    }

    #[test]
    fn subscribe_ack_builder_writes_the_reboot_flag_it_is_given() {
        let request = SubscribeAckRequest {
            service_id: 0x0042,
            instance_id: 1,
            major_version: 1,
            event_group_id: 1,
            ttl: 3,
            counter: 0,
        };
        for reboot in [RebootFlag::RecentlyRebooted, RebootFlag::Continuous] {
            let mut buf = [0u8; 64];
            let len = build_subscribe_ack_datagram(&mut buf, &request, 2, reboot).unwrap();
            let view = parse_someip_sd_datagram(&buf[..len]).expect("valid SD datagram");
            assert_eq!(view.flags().reboot(), reboot);
        }
    }

    #[test]
    fn subscribe_ack_echoes_the_subscribe_counter() {
        let request = SubscribeAckRequest {
            service_id: 0x0042,
            instance_id: 1,
            major_version: 1,
            event_group_id: 1,
            ttl: 3,
            counter: 3,
        };
        let mut buf = [0u8; 64];
        let len =
            build_subscribe_ack_datagram(&mut buf, &request, 2, RebootFlag::Continuous).unwrap();
        let view = parse_someip_sd_datagram(&buf[..len]).expect("valid SD datagram");
        assert_eq!(view.entries().next().expect("one entry").counter(), 3);
    }
}
