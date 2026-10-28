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
use core::sync::atomic::{AtomicU32, Ordering};

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

/// One `SubscribeAckEventgroup` entry. The service, instance, eventgroup,
/// major version and counter echo the `Subscribe` it answers; `ttl` is the
/// lifetime the server grants, 0 for a `SubscribeNack`.
#[derive(Clone, Copy, Debug)]
pub struct SubscribeAckRequest {
    pub service_id: u16,
    pub instance_id: u16,
    pub event_group_id: u16,
    pub major_version: u8,
    pub ttl: u32,
    /// The counter of the `Subscribe` being answered.
    pub counter: SubscribeCounter,
}

/// The 4-bit counter of a `SubscribeEventgroup` entry, which tells apart
/// Subscribes to one eventgroup that differ only in their endpoint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubscribeCounter(u8);

impl SubscribeCounter {
    /// The largest counter value.
    pub const MAX: u8 = 0x0F;

    /// The counter `value`, or `None` if it is above [`Self::MAX`].
    #[must_use]
    pub const fn new(value: u8) -> Option<Self> {
        if value <= Self::MAX {
            Some(Self(value))
        } else {
            None
        }
    }

    /// The counter of `entry`, a `SubscribeEventgroup` entry.
    #[must_use]
    pub fn of_entry(entry: &crate::protocol::sd::EntryView<'_>) -> Self {
        Self(entry.counter().to_be_bytes()[1] & Self::MAX)
    }

    /// The counter's value, at most [`Self::MAX`].
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

/// Packet-construction errors.
#[derive(Debug)]
#[non_exhaustive]
pub enum BuildError {
    /// `buf` is shorter than the encoded datagram. Carries the codec's
    /// [`InsufficientBuffer`] so callers see the needed / available byte
    /// counts, matching the rest of the crate's buffer-too-small reporting.
    BufferTooSmall(InsufficientBuffer),
    /// A fixed-capacity entry or option list could not take another element.
    ///
    /// Its fields count elements, not bytes as in
    /// [`BufferTooSmall`](Self::BufferTooSmall).
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
    /// other than [`BuildError::InvalidTtl`]; carries the encoder's error.
    Encode(crate::protocol::Error),
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
            BuildError::Encode(_) => f.write_str("encoding failed"),
        }
    }
}

impl core::error::Error for BuildError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            BuildError::Encode(e) => Some(e),
            _ => None,
        }
    }
}

impl From<InsufficientBuffer> for BuildError {
    fn from(ib: InsufficientBuffer) -> Self {
        BuildError::BufferTooSmall(ib)
    }
}

impl From<EncodeToSliceError<crate::protocol::Error>> for BuildError {
    fn from(e: EncodeToSliceError<crate::protocol::Error>) -> Self {
        match e {
            EncodeToSliceError::InsufficientBuffer(ib) => BuildError::BufferTooSmall(ib),
            EncodeToSliceError::Encode(e) => encode_error(e),
        }
    }
}

fn encode_error(e: crate::protocol::Error) -> BuildError {
    match e {
        crate::protocol::Error::Sd(crate::protocol::sd::Error::InvalidTtl(ttl)) => {
            BuildError::InvalidTtl(ttl)
        }
        e => BuildError::Encode(e),
    }
}

/// Encode a `SubscribeEventgroup` datagram into `buf`, numbered by
/// `session`. Returns its length in bytes.
///
/// # Errors
/// See [`BuildError`].
pub fn build_subscribe_eventgroup_datagram(
    buf: &mut [u8],
    request: &SubscribeEventgroupRequest,
    session: SdSession,
) -> Result<usize, BuildError> {
    build_multi_subscribe_eventgroup_datagram::<1>(buf, core::slice::from_ref(request), session)
}

/// SD header length in bytes: flags and reserved bytes, entries length,
/// options length.
#[cfg(feature = "server")]
const SD_HEADER_LEN: usize = 12;

/// Length of the SD datagram [`build_multi_subscribe_eventgroup_datagram`]
/// encodes for `requests` Subscribe entries.
#[cfg(feature = "server")]
pub(crate) const fn multi_subscribe_datagram_len(requests: usize) -> usize {
    SOMEIP_HEADER_LEN
        + SD_HEADER_LEN
        + requests * (crate::protocol::sd::ENTRY_SIZE + crate::protocol::sd::IPV4_OPTION_WIRE_SIZE)
}

/// Length of the SD datagram [`build_find_service_datagram`] encodes for
/// `instances` service instances.
#[cfg(feature = "server")]
pub(crate) const fn find_service_datagram_len(instances: usize) -> usize {
    SOMEIP_HEADER_LEN + SD_HEADER_LEN + instances * crate::protocol::sd::ENTRY_SIZE
}

/// Encode a single SD datagram carrying one `SubscribeEventgroup` entry per
/// element of `requests`, each referencing its own IPv4 endpoint option,
/// numbered by `session`.
///
/// # Errors
/// Returns [`BuildError::ListFull`] if `requests` holds more than `N`
/// elements; otherwise see [`BuildError`].
pub fn build_multi_subscribe_eventgroup_datagram<const N: usize>(
    buf: &mut [u8],
    requests: &[SubscribeEventgroupRequest],
    session: SdSession,
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
    encode_sd_datagram(buf, &entries, &options, session)
}

/// Encode an `OfferService` datagram into `buf`, numbered by `session`.
/// Returns its length in bytes.
///
/// # Errors
/// See [`BuildError`].
pub fn build_offer_service_datagram(
    buf: &mut [u8],
    request: &OfferServiceRequest,
    session: SdSession,
) -> Result<usize, BuildError> {
    encode_service_entry_datagram(buf, request, session, /*stop=*/ false)
}

/// Encode a `StopOfferService` datagram into `buf` (same shape as
/// [`build_offer_service_datagram`], TTL forced to `0`), numbered by
/// `session`.
///
/// # Errors
/// See [`BuildError`].
pub fn build_stop_offer_service_datagram(
    buf: &mut [u8],
    request: &OfferServiceRequest,
    session: SdSession,
) -> Result<usize, BuildError> {
    encode_service_entry_datagram(buf, request, session, /*stop=*/ true)
}

/// Encode a single SD datagram carrying one `OfferService` entry per
/// element of `requests` (up to `N`). Each entry references its own
/// IPv4 endpoint option at the matching flat index — one coherent SD
/// message instead of one packet per service. Numbered by `session`.
///
/// # Errors
/// See [`BuildError`].
pub fn build_multi_offer_service_datagram<const N: usize>(
    buf: &mut [u8],
    requests: &[OfferServiceRequest],
    session: SdSession,
) -> Result<usize, BuildError> {
    build_multi_service_entry_datagram::<N>(buf, requests, session, /*stop=*/ false)
}

/// Encode a single SD datagram carrying one `StopOfferService` entry per
/// element of `requests` (up to `N`), numbered by `session`.
///
/// # Errors
/// See [`build_multi_offer_service_datagram`].
pub fn build_multi_stop_offer_service_datagram<const N: usize>(
    buf: &mut [u8],
    requests: &[OfferServiceRequest],
    session: SdSession,
) -> Result<usize, BuildError> {
    build_multi_service_entry_datagram::<N>(buf, requests, session, /*stop=*/ true)
}

fn build_multi_service_entry_datagram<const N: usize>(
    buf: &mut [u8],
    requests: &[OfferServiceRequest],
    session: SdSession,
    stop: bool,
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
    encode_sd_datagram(buf, &entries, &options, session)
}

/// Encode one SD datagram with a `FindService` entry, any minor version and
/// no options, for each element of `wanted`.
#[cfg(feature = "server")]
pub(crate) fn build_find_service_datagram<const N: usize>(
    buf: &mut [u8],
    wanted: &[SubscribeEventgroupRequest],
    session: SdSession,
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
    encode_sd_datagram(buf, &entries, &[], session)
}

/// Encode a `SubscribeAckEventgroup` datagram into `buf`. Caller
/// transmits to the original `Subscribe` sender's SD endpoint. Numbered by
/// `session`.
///
/// # Errors
/// See [`BuildError`].
pub fn build_subscribe_ack_datagram(
    buf: &mut [u8],
    request: &SubscribeAckRequest,
    session: SdSession,
) -> Result<usize, BuildError> {
    let entry = Entry::SubscribeAckEventGroup(EventGroupEntry {
        index_first_options_run: 0,
        index_second_options_run: 0,
        options_count: OptionsCount::new(0, 0),
        service_id: request.service_id,
        instance_id: request.instance_id,
        major_version: request.major_version,
        ttl: request.ttl,
        counter: u16::from(request.counter.get()),
        event_group_id: request.event_group_id,
    });
    encode_sd_datagram(buf, &[entry], &[], session)
}

fn encode_service_entry_datagram(
    buf: &mut [u8],
    request: &OfferServiceRequest,
    session: SdSession,
    stop: bool,
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
    encode_sd_datagram(buf, &[entry], &[option], session)
}

/// Encode `entries` + `options` as an SD payload, prefixed with the
/// SOME/IP wrapper built by [`Header::new_sd`]. Request ID is
/// `client_id=0 | session`.
fn encode_sd_datagram(
    buf: &mut [u8],
    entries: &[Entry],
    options: &[SdOptions],
    session: SdSession,
) -> Result<usize, BuildError> {
    // Header-first, body-second: `sd::Header::encoded_size()` is exact and
    // computable without writing, so the SOME/IP length field is known before
    // either section is written. One linear forward pass — no backfill.
    let sd_payload = SdHeader::new(Flags::new_sd(session.reboot()), entries, options);
    let sd_payload_len = sd_payload.encoded_size().map_err(encode_error)?;
    let total = SOMEIP_HEADER_LEN + sd_payload_len;
    if buf.len() < total {
        return Err(BuildError::BufferTooSmall(InsufficientBuffer {
            needed: total,
            available: buf.len(),
        }));
    }

    let header = Header::new_sd(u32::from(session.id()), sd_payload_len);
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
/// [`BuildError::Encode`] if header encoding fails.
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
        .map_err(BuildError::from)?;
    Ok(())
}

/// The session ID and reboot flag of one SD message, taken together from an
/// [`SdSessionCounter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SdSession {
    id: u16,
    reboot: RebootFlag,
}

impl SdSession {
    /// The session ID, 1 to 0xFFFF.
    #[must_use]
    pub const fn id(self) -> u16 {
        self.id
    }

    /// The reboot flag: [`RebootFlag::RecentlyRebooted`] until the counter
    /// first wraps, [`RebootFlag::Continuous`] after.
    #[must_use]
    pub const fn reboot(self) -> RebootFlag {
        self.reboot
    }
}

/// Numbers SD messages. Session IDs count from 1 and wrap from 0xFFFF to 1;
/// the reboot flag is [`RebootFlag::RecentlyRebooted`] until the first wrap
/// and [`RebootFlag::Continuous`] from then on. Concurrent callers of
/// [`Self::next`] each get a distinct session.
#[derive(Debug)]
pub struct SdSessionCounter {
    /// Bits 0..16: the session ID last given, or 0 before the first.
    /// Bit 16: set once the counter has wrapped.
    state: AtomicU32,
}

const SID_MASK: u32 = 0xFFFF;
const WRAPPED_BIT: u32 = 1 << 16;

impl SdSessionCounter {
    /// A counter whose first session is ID 1, recently rebooted.
    #[must_use]
    pub const fn new() -> Self {
        Self::with_initial(0)
    }

    /// A counter that has given `last` and not yet wrapped: the next session
    /// is `last + 1`, or 1 with [`RebootFlag::Continuous`] after 0xFFFF.
    /// 0 is the same as [`Self::new`].
    #[must_use]
    pub const fn with_initial(last: u16) -> Self {
        Self {
            state: AtomicU32::new(last as u32),
        }
    }

    /// The next session.
    pub fn next(&self) -> SdSession {
        // `fetch_update` is deprecated on nightly in favour of `try_update`,
        // which is not yet on stable.
        #[allow(deprecated)]
        let (Ok(prev) | Err(prev)) =
            self.state
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                    Some(Self::after(state))
                });
        let next = Self::after(prev);
        #[allow(clippy::cast_possible_truncation)]
        let id = (next & SID_MASK) as u16;
        SdSession {
            id,
            reboot: if next & WRAPPED_BIT == 0 {
                RebootFlag::RecentlyRebooted
            } else {
                RebootFlag::Continuous
            },
        }
    }

    /// Whether the counter has wrapped.
    #[cfg(test)]
    pub(crate) fn has_wrapped(&self) -> bool {
        self.state.load(Ordering::Acquire) & WRAPPED_BIT != 0
    }

    const fn after(state: u32) -> u32 {
        let last = state & SID_MASK;
        let wrapped = state & WRAPPED_BIT != 0 || last == SID_MASK;
        let id = if last == SID_MASK { 1 } else { last + 1 };
        id | if wrapped { WRAPPED_BIT } else { 0 }
    }
}

impl Default for SdSessionCounter {
    fn default() -> Self {
        Self::new()
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

    /// The session with ID `id` and reboot flag `reboot`, from a counter.
    fn session(id: u16, reboot: RebootFlag) -> SdSession {
        let counter = match reboot {
            RebootFlag::RecentlyRebooted => return SdSessionCounter::with_initial(id - 1).next(),
            RebootFlag::Continuous => SdSessionCounter::with_initial(u16::MAX),
        };
        loop {
            let s = counter.next();
            if s.id() == id {
                return s;
            }
        }
    }

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
        assert!(matches!(
            build_offer_service_datagram(&mut buf, &request, session(1, RebootFlag::Continuous)),
            Err(BuildError::InvalidTtl(0))
        ));
        assert!(matches!(
            build_multi_offer_service_datagram::<4>(
                &mut buf,
                &[request],
                session(1, RebootFlag::Continuous)
            ),
            Err(BuildError::InvalidTtl(0))
        ));
    }

    #[test]
    fn stop_offer_is_built_whatever_the_requested_ttl() {
        let mut buf = [0u8; 512];
        for ttl in [3, u32::MAX] {
            let request = OfferServiceRequest {
                ttl,
                ..req(0x0001, 30501)
            };
            let len = build_stop_offer_service_datagram(
                &mut buf,
                &request,
                session(1, RebootFlag::Continuous),
            )
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
        let len = build_multi_offer_service_datagram::<8>(
            &mut buf,
            &offers,
            session(7, RebootFlag::Continuous),
        )
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
        let err = build_subscribe_eventgroup_datagram(
            &mut buf,
            &request,
            session(3, RebootFlag::Continuous),
        )
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
        let sd_len = build_subscribe_eventgroup_datagram(
            &mut sd_buf,
            &request,
            session(3, RebootFlag::Continuous),
        )
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
        let len = build_offer_service_datagram(
            &mut buf,
            &req(0x0001, 30501),
            session(1, RebootFlag::Continuous),
        )
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
        let sd_len = build_subscribe_eventgroup_datagram(
            &mut sd_buf,
            &request,
            session(3, RebootFlag::Continuous),
        )
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
            let len = build_subscribe_eventgroup_datagram(&mut buf, &request, session(3, reboot))
                .unwrap();
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
            session(3, RebootFlag::Continuous),
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
                session(3, RebootFlag::Continuous),
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
        assert!(matches!(
            build_multi_subscribe_eventgroup_datagram::<1>(
                &mut buf,
                &[request, request],
                session(3, RebootFlag::Continuous)
            ),
            Err(BuildError::ListFull {
                needed: 2,
                capacity: 1
            })
        ));
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
            session(5, RebootFlag::Continuous),
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
        assert_eq!(len, find_service_datagram_len(1));
    }

    #[cfg(feature = "server")]
    #[test]
    fn find_rejects_more_instances_than_its_capacity() {
        let mut buf = [0u8; 64];
        assert!(matches!(
            build_find_service_datagram::<1>(
                &mut buf,
                &[find_request(), find_request()],
                session(5, RebootFlag::Continuous)
            ),
            Err(BuildError::ListFull {
                needed: 2,
                capacity: 1
            })
        ));
    }

    #[test]
    fn an_encode_error_is_the_build_errors_source() {
        let err = BuildError::from(EncodeToSliceError::Encode(crate::protocol::Error::from(
            crate::protocol::sd::Error::InvalidOptionType(0x77),
        )));
        let source = core::error::Error::source(&err).expect("a source");
        assert!(source.downcast_ref::<crate::protocol::Error>().is_some());
    }

    #[test]
    fn a_new_counter_starts_at_one_recently_rebooted() {
        let s = SdSessionCounter::new().next();
        assert_eq!((s.id(), s.reboot()), (1, RebootFlag::RecentlyRebooted));
    }

    #[test]
    fn the_counter_wraps_to_one_and_stays_continuous() {
        let counter = SdSessionCounter::with_initial(0xFFFE);
        let sessions: [_; 3] = core::array::from_fn(|_| {
            let s = counter.next();
            (s.id(), s.reboot())
        });
        assert_eq!(
            sessions,
            [
                (0xFFFF, RebootFlag::RecentlyRebooted),
                (1, RebootFlag::Continuous),
                (2, RebootFlag::Continuous)
            ]
        );
    }

    #[test]
    fn offer_builders_write_the_reboot_flag_they_are_given() {
        type Builder = fn(&mut [u8], &OfferServiceRequest, RebootFlag) -> Result<usize, BuildError>;
        let builders: [(&str, Builder); 4] = [
            ("offer", |b, r, f| {
                build_offer_service_datagram(b, r, session(1, f))
            }),
            ("stop", |b, r, f| {
                build_stop_offer_service_datagram(b, r, session(1, f))
            }),
            ("multi offer", |b, r, f| {
                build_multi_offer_service_datagram::<1>(b, core::slice::from_ref(r), session(1, f))
            }),
            ("multi stop", |b, r, f| {
                build_multi_stop_offer_service_datagram::<1>(
                    b,
                    core::slice::from_ref(r),
                    session(1, f),
                )
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
        assert!(matches!(
            build_multi_offer_service_datagram::<1>(
                &mut buf,
                &requests,
                session(1, RebootFlag::Continuous)
            ),
            Err(BuildError::ListFull {
                needed: 2,
                capacity: 1
            })
        ));
        assert!(matches!(
            build_multi_stop_offer_service_datagram::<1>(
                &mut buf,
                &requests,
                session(1, RebootFlag::Continuous)
            ),
            Err(BuildError::ListFull {
                needed: 2,
                capacity: 1
            })
        ));
    }

    #[test]
    fn subscribe_ack_builder_writes_the_reboot_flag_it_is_given() {
        let request = SubscribeAckRequest {
            service_id: 0x0042,
            instance_id: 1,
            major_version: 1,
            event_group_id: 1,
            ttl: 3,
            counter: SubscribeCounter::default(),
        };
        for reboot in [RebootFlag::RecentlyRebooted, RebootFlag::Continuous] {
            let mut buf = [0u8; 64];
            let len = build_subscribe_ack_datagram(&mut buf, &request, session(2, reboot)).unwrap();
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
            counter: SubscribeCounter::new(3).unwrap(),
        };
        let mut buf = [0u8; 64];
        let len =
            build_subscribe_ack_datagram(&mut buf, &request, session(2, RebootFlag::Continuous))
                .unwrap();
        let view = parse_someip_sd_datagram(&buf[..len]).expect("valid SD datagram");
        assert_eq!(view.entries().next().expect("one entry").counter(), 3);
    }

    #[test]
    fn a_subscribe_counter_holds_at_most_four_bits() {
        assert_eq!(
            SubscribeCounter::new(SubscribeCounter::MAX).map(SubscribeCounter::get),
            Some(0x0F)
        );
        assert_eq!(SubscribeCounter::new(0x10), None);
    }

    #[test]
    fn a_subscribe_counter_read_from_an_entry_keeps_only_the_counter_bits() {
        let mut entry = [0u8; 16];
        entry[12..14].copy_from_slice(&[0x80, 0x37]);
        let (view, _) =
            <crate::protocol::sd::EntryView as crate::Decode>::decode(&entry).expect("an entry");
        assert_eq!(SubscribeCounter::of_entry(&view).get(), 0x07);
    }
}
