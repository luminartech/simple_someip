//! Synchronous polled SOME/IP helpers for bare-metal targets.
//!
//! Gated by `feature = "bare_metal_poll"`. Transport-agnostic SD
//! packet builders + SOMEIP datagram parsers driven from a caller's
//! periodic tick instead of the async `Client`/`Server` paths.
//!
//! See `docs/polled-bare-metal-rationale.md` for the memory-cost
//! comparison that motivates this module.

use core::net::{Ipv4Addr, SocketAddrV4};
use core::sync::atomic::{AtomicU16, AtomicU32, Ordering};

use crate::E2ECheckStatus;
use crate::E2EKey;
use crate::StaticE2EHandle;
use crate::WireFormat;
use crate::protocol::{HEADER_SIZE, Header, HeaderView, MessageId};
use crate::protocol::sd::{
    Entry, EntryType, EventGroupEntry, Flags, Header as SdHeader, Options as SdOptions,
    OptionsCount, RebootFlag, SdHeaderView, ServiceEntry, TransportProtocol,
};
use crate::server::{StaticSubscriptionHandle, SubscriptionHandle};
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

/// Encode a single SD datagram carrying one `OfferService` entry
/// per element of `requests` (up to `N` entries). Each entry
/// references its own IPv4 endpoint option; entries are placed in
/// `requests` order.
///
/// Using one packet for all offered services is more efficient and
/// matches how commercial SOME/IP stacks behave.
///
/// # Errors
/// Returns [`BuildError::BufferTooSmall`] if `buf` is too short or
/// if `requests.len()` exceeds the const cap `N`.
pub fn build_multi_offer_service_datagram<const N: usize>(
    buf: &mut [u8],
    requests: &[OfferServiceRequest],
    session: u16,
) -> Result<usize, BuildError> {
    build_multi_service_entry_datagram::<N>(buf, requests, session, /*stop=*/ false)
}

/// Encode a single SD datagram carrying one `StopOfferService` entry
/// per element of `requests` (up to `N` entries).
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
        let svc = ServiceEntry {
            // Each entry owns exactly one option at position `i`
            // in the flat options array.
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
    encode_sd_datagram(buf, &entries, &options, session)
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
pub fn check_parsed_e2e<'a, const E2E_CAP: usize>(
    e2e: &StaticE2EHandle<E2E_CAP>,
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

// ─────────────────────────────────────────────────────────────────────
// Single-call orchestrator (`tick`)
// ─────────────────────────────────────────────────────────────────────
//
// Below: a higher-level API that lets a bare-metal consumer drive
// the full polled SOME/IP integration from one call per scheduler
// tick. The granular helpers above remain available for callers who
// want finer control.

/// One offered service entry. Drives both periodic `OfferService`
/// announces and inbound `Subscribe` dispatch.
#[derive(Clone, Copy, Debug)]
pub struct Offer {
    pub service_id: u16,
    pub instance_id: u16,
    pub event_group_id: u16,
    pub major_version: u8,
    /// `OfferService` TTL in seconds.
    pub ttl_seconds: u32,
    /// Local UDP port the service is bound to.
    pub unicast_port: u16,
}

/// One subscription the consumer wants to maintain against a remote
/// service.
#[derive(Clone, Copy, Debug)]
pub struct Subscription {
    pub service_id: u16,
    pub instance_id: u16,
    pub event_group_id: u16,
    pub major_version: u8,
    /// Local UDP port the consumer listens on for event delivery.
    pub local_rx_port: u16,
    /// `SubscribeEventgroup` TTL in seconds (24-bit).
    pub ttl_seconds: u32,
}

/// Mutable state owned by the consumer, threaded into every `tick`
/// call. The two atomics inside cover the periodic-emit cadence and
/// the AUTOSAR-SD session counter for outbound packets.
#[derive(Debug, Default)]
pub struct PeriodicState {
    pub session: AtomicU16,
    pub last_emit_ms: AtomicU32,
}

impl PeriodicState {
    /// Initial state: session 1, last-emit 0 (fires on first tick).
    pub const fn new() -> Self {
        Self {
            session: AtomicU16::new(1),
            last_emit_ms: AtomicU32::new(0),
        }
    }
}

/// Configuration for [`tick`]. All fields are caller-owned;
/// `sd_scratch` is borrowed mutably for the SD-send path.
pub struct PolledConfig<'a> {
    pub local_ip: Ipv4Addr,
    pub sd_endpoint: SocketAddrV4,
    pub offers: &'a [Offer],
    pub subscriptions: &'a [Subscription],
    pub offer_period_ms: u32,
    pub subscribe_period_ms: u32,
    /// Scratch buffer for outbound SD datagrams (Offers, Subscribes,
    /// SubscribeAcks). One of these encodes at a time; ~256 B is
    /// enough for a single-entry SD packet.
    pub sd_scratch: &'a mut [u8],
}

/// Borrowed view over one pending inbound datagram pulled from the
/// consumer's inbox.
///
/// Built by the consumer's `recv` closure (see
/// [`DatagramRef::new`]). The `release`/`cookie` pair runs on drop
/// to release the underlying inbox slot — typical use is to flip a
/// `has_datagram` atomic so the slot can be reused.
pub struct DatagramRef {
    data_ptr: *const u8,
    data_len: usize,
    src: SocketAddrV4,
    cookie: *mut (),
    release: unsafe fn(*mut ()),
}

impl DatagramRef {
    /// Construct a [`DatagramRef`] over caller-owned bytes.
    ///
    /// # Safety
    /// - `data` must remain valid (no concurrent writes) until the
    ///   returned [`DatagramRef`] is dropped.
    /// - `release` must be safe to call exactly once with `cookie`,
    ///   and must make the inbox slot reusable afterwards.
    #[must_use]
    pub unsafe fn new(
        data: &[u8],
        src: SocketAddrV4,
        cookie: *mut (),
        release: unsafe fn(*mut ()),
    ) -> Self {
        Self {
            data_ptr: data.as_ptr(),
            data_len: data.len(),
            src,
            cookie,
            release,
        }
    }

    /// Datagram payload bytes. The returned slice is tied to
    /// `&self` and cannot outlive the [`DatagramRef`].
    #[must_use]
    pub fn data(&self) -> &[u8] {
        // SAFETY: `new`'s contract holds `data_ptr..len` valid for
        // the lifetime of `self`.
        unsafe { core::slice::from_raw_parts(self.data_ptr, self.data_len) }
    }

    /// Source `(IP, port)` the datagram was received from.
    #[must_use]
    pub fn src(&self) -> SocketAddrV4 {
        self.src
    }
}

impl Drop for DatagramRef {
    fn drop(&mut self) {
        // SAFETY: `new`'s contract holds `release(cookie)` safe.
        unsafe { (self.release)(self.cookie) }
    }
}

/// Drive one cycle of polled SOME/IP work: drain pending inbound
/// datagrams on the SD / offered-service / subscribed-service
/// ports, register subscribers + emit `SubscribeAck` for matching
/// inbound Subscribes, dispatch unicast requests and E2E-checked
/// events to `dispatch`, and emit periodic `OfferService` /
/// `SubscribeEventgroup` packets if the configured periods have
/// elapsed.
///
/// Callbacks:
/// - `recv(port) -> Option<DatagramRef>` pulls one pending inbound
///   datagram from the consumer's inbox for `port`; dropping the
///   returned ref releases the underlying slot.
/// - `send(buf, dst)` transmits a datagram to `dst`.
/// - `dispatch(service_id, method_id, payload, e2e_status)`
///   forwards a parsed inbound message to the consumer.
///
/// All three are `FnMut` so consumers can capture state in their
/// closures.
pub fn tick<
    const EG: usize,
    const SUBS: usize,
    const E2E_CAP: usize,
    FRecv,
    FSend,
    FDispatch,
>(
    now_ms: u32,
    config: &mut PolledConfig<'_>,
    server_state: &PeriodicState,
    client_state: &PeriodicState,
    e2e: &StaticE2EHandle<E2E_CAP>,
    subs: &StaticSubscriptionHandle<EG, SUBS>,
    mut recv: FRecv,
    mut send: FSend,
    mut dispatch: FDispatch,
) where
    FRecv: FnMut(u16) -> Option<DatagramRef>,
    FSend: FnMut(&[u8], SocketAddrV4),
    FDispatch: FnMut(u16, u16, &[u8], u8),
{
    drain_sd_inbox(config, server_state, subs, &mut recv, &mut send);
    drain_offered_unicast_inboxes(config.offers, &mut recv, &mut dispatch);
    drain_subscribed_event_inboxes(config.subscriptions, e2e, &mut recv, &mut dispatch);
    maybe_emit_offers(now_ms, config, server_state, &mut send);
    maybe_emit_subscribes(now_ms, config, client_state, &mut send);
}

/// Emit a `StopOfferService` for every entry in `offers` (one
/// packet per offer). Intended for shutdown — consumer typically
/// calls this from its `deinit` path so peers drop the
/// registration immediately instead of waiting for TTL.
pub fn emit_stop_offers<FSend>(
    config: &mut PolledConfig<'_>,
    server_state: &PeriodicState,
    mut send: FSend,
) where
    FSend: FnMut(&[u8], SocketAddrV4),
{
    let requests: heapless::Vec<OfferServiceRequest, MAX_OFFERS_PER_SD> = config
        .offers
        .iter()
        .map(|offer| OfferServiceRequest {
            service_id: offer.service_id,
            instance_id: offer.instance_id,
            major_version: offer.major_version,
            minor_version: 0,
            ttl: 0,
            local_ip: config.local_ip,
            unicast_port: offer.unicast_port,
        })
        .collect();
    let session = next_sd_session(&server_state.session);
    if let Ok(len) =
        build_multi_stop_offer_service_datagram::<MAX_OFFERS_PER_SD>(config.sd_scratch, &requests, session)
    {
        send(&config.sd_scratch[..len], config.sd_endpoint);
    }
}

fn drain_sd_inbox<const EG: usize, const SUBS: usize, FRecv, FSend>(
    config: &mut PolledConfig<'_>,
    server_state: &PeriodicState,
    subs: &StaticSubscriptionHandle<EG, SUBS>,
    recv: &mut FRecv,
    send: &mut FSend,
) where
    FRecv: FnMut(u16) -> Option<DatagramRef>,
    FSend: FnMut(&[u8], SocketAddrV4),
{
    while let Some(datagram) = recv(config.sd_endpoint.port()) {
        if let Some(view) = parse_someip_sd_datagram(datagram.data()) {
            process_sd_inbound(
                view,
                datagram.src(),
                config.offers,
                subs,
                config.sd_scratch,
                &server_state.session,
                |buf, dst| send(buf, dst),
            );
        }
        // `datagram` drops here -> slot released back to inbox.
    }
}

fn drain_offered_unicast_inboxes<FRecv, FDispatch>(
    offers: &[Offer],
    recv: &mut FRecv,
    dispatch: &mut FDispatch,
) where
    FRecv: FnMut(u16) -> Option<DatagramRef>,
    FDispatch: FnMut(u16, u16, &[u8], u8),
{
    // Visit each unique unicast port once; multiple offers may
    // share a port (one server, several services).
    let mut visited: heapless::Vec<u16, MAX_UNIQUE_PORTS> = heapless::Vec::new();
    for offer in offers {
        if visited.iter().any(|&p| p == offer.unicast_port) {
            continue;
        }
        // Best-effort push; if more unique ports than the cap, we
        // drain what we can. (`MAX_UNIQUE_PORTS = 16` covers any
        // sane catalog.)
        let _ = visited.push(offer.unicast_port);
        while let Some(datagram) = recv(offer.unicast_port) {
            if let Some(parsed) = parse_someip_datagram(datagram.data()) {
                // Unicast requests aren't E2E-protected in this
                // module's contract — surface with `Unchecked`.
                dispatch(
                    parsed.service_id,
                    parsed.method_id,
                    parsed.payload,
                    E2ECheckStatus::Unchecked.to_return_code(),
                );
            }
        }
    }
}

fn drain_subscribed_event_inboxes<const E2E_CAP: usize, FRecv, FDispatch>(
    subscriptions: &[Subscription],
    e2e: &StaticE2EHandle<E2E_CAP>,
    recv: &mut FRecv,
    dispatch: &mut FDispatch,
) where
    FRecv: FnMut(u16) -> Option<DatagramRef>,
    FDispatch: FnMut(u16, u16, &[u8], u8),
{
    // Same per-port dedup as the unicast drain.
    let mut visited: heapless::Vec<u16, MAX_UNIQUE_PORTS> = heapless::Vec::new();
    for sub in subscriptions {
        if visited.iter().any(|&p| p == sub.local_rx_port) {
            continue;
        }
        let _ = visited.push(sub.local_rx_port);
        while let Some(datagram) = recv(sub.local_rx_port) {
            if let Some(parsed) = parse_someip_datagram(datagram.data()) {
                let (status, body) = check_parsed_e2e(e2e, &parsed);
                dispatch(
                    parsed.service_id,
                    parsed.method_id,
                    body,
                    status.to_return_code(),
                );
            }
        }
    }
}

fn maybe_emit_offers<FSend>(
    now_ms: u32,
    config: &mut PolledConfig<'_>,
    server_state: &PeriodicState,
    send: &mut FSend,
) where
    FSend: FnMut(&[u8], SocketAddrV4),
{
    if !period_elapsed(now_ms, &server_state.last_emit_ms, config.offer_period_ms) {
        return;
    }
    // Build one SD packet that carries all OfferService entries.
    let requests: heapless::Vec<OfferServiceRequest, MAX_OFFERS_PER_SD> = config
        .offers
        .iter()
        .map(|offer| OfferServiceRequest {
            service_id: offer.service_id,
            instance_id: offer.instance_id,
            major_version: offer.major_version,
            minor_version: 0,
            ttl: offer.ttl_seconds,
            local_ip: config.local_ip,
            unicast_port: offer.unicast_port,
        })
        .collect();
    let session = next_sd_session(&server_state.session);
    if let Ok(len) =
        build_multi_offer_service_datagram::<MAX_OFFERS_PER_SD>(config.sd_scratch, &requests, session)
    {
        send(&config.sd_scratch[..len], config.sd_endpoint);
    }
}

fn maybe_emit_subscribes<FSend>(
    now_ms: u32,
    config: &mut PolledConfig<'_>,
    client_state: &PeriodicState,
    send: &mut FSend,
) where
    FSend: FnMut(&[u8], SocketAddrV4),
{
    if !period_elapsed(now_ms, &client_state.last_emit_ms, config.subscribe_period_ms) {
        return;
    }
    for sub in config.subscriptions {
        let request = SubscribeEventgroupRequest {
            service_id: sub.service_id,
            instance_id: sub.instance_id,
            major_version: sub.major_version,
            event_group_id: sub.event_group_id,
            ttl: sub.ttl_seconds,
            local_ip: config.local_ip,
            local_rx_port: sub.local_rx_port,
        };
        let session = next_sd_session(&client_state.session);
        if let Ok(len) =
            build_subscribe_eventgroup_datagram(config.sd_scratch, &request, session)
        {
            send(&config.sd_scratch[..len], config.sd_endpoint);
        }
    }
}

/// Returns `true` and updates `last` when `now_ms` is at least
/// `period_ms` past the previous emit (wrapping `u32` arithmetic).
fn period_elapsed(now_ms: u32, last: &AtomicU32, period_ms: u32) -> bool {
    let prev = last.load(Ordering::Relaxed);
    if now_ms.wrapping_sub(prev) < period_ms {
        return false;
    }
    last.store(now_ms, Ordering::Relaxed);
    true
}

/// Dispatch one inbound SOME/IP-SD payload: snapshot options,
/// iterate Subscribe entries, register matching ones in `subs`,
/// emit a SubscribeAck for each via `send`.
///
/// Subscribes that don't match an entry in `offers` are dropped
/// silently — protects the subscription registry from accepting
/// requests for services we don't offer.
fn process_sd_inbound<const EG: usize, const SUBS: usize, FSend>(
    view: SdHeaderView<'_>,
    peer_sd: SocketAddrV4,
    offers: &[Offer],
    subs: &StaticSubscriptionHandle<EG, SUBS>,
    sd_scratch: &mut [u8],
    ack_session: &AtomicU16,
    mut send: FSend,
) where
    FSend: FnMut(&[u8], SocketAddrV4),
{
    // Typical SD packets carry ≤ 2 IPv4 endpoint options; cap at 8
    // to leave headroom without blowing stack.
    let mut options_buf: [Option<SdOptions>; MAX_INBOUND_SD_OPTIONS] =
        [const { None }; MAX_INBOUND_SD_OPTIONS];
    let mut opt_count: usize = 0;
    for opt_view in view.options() {
        if opt_count >= options_buf.len() {
            break;
        }
        if let Ok(o) = opt_view.to_owned() {
            options_buf[opt_count] = Some(o);
            opt_count += 1;
        }
    }

    for entry_view in view.entries() {
        let Ok(et) = entry_view.entry_type() else { continue };
        if et != EntryType::Subscribe {
            continue;
        }
        let svc = entry_view.service_id();
        let inst = entry_view.instance_id();
        let eg = entry_view.event_group_id();
        // Drop Subscribes that don't match anything in our offers
        // table — otherwise we'd ack on behalf of services we
        // never offered.
        if !offers
            .iter()
            .any(|o| o.service_id == svc && o.instance_id == inst && o.event_group_id == eg)
        {
            continue;
        }
        let major = entry_view.major_version();
        let ttl = entry_view.ttl();

        // Subscriber's notification endpoint is the first IPv4
        // endpoint option; fall back to the SD source if absent.
        let subscriber_addr = options_buf[..opt_count]
            .iter()
            .find_map(|slot| match slot {
                Some(SdOptions::IpV4Endpoint { ip, port, .. }) => {
                    Some(SocketAddrV4::new(*ip, *port))
                }
                _ => None,
            })
            .unwrap_or(peer_sd);

        // Register the subscriber. `StaticSubscriptionHandle`
        // returns `core::future::Ready`, so `into_inner` extracts
        // the result without polling.
        let _ = subs.subscribe(svc, inst, eg, subscriber_addr).into_inner();

        // Ack to the peer's SD endpoint.
        let session = next_sd_session(ack_session);
        let request = SubscribeAckRequest {
            service_id: svc,
            instance_id: inst,
            event_group_id: eg,
            major_version: major,
            ttl,
        };
        if let Ok(len) = build_subscribe_ack_datagram(sd_scratch, &request, session) {
            send(&sd_scratch[..len], peer_sd);
        }
    }
}

const MAX_INBOUND_SD_OPTIONS: usize = 8;
/// Cap on the number of `OfferService` entries packed into one SD
/// datagram. 16 comfortably covers any realistic service catalog.
const MAX_OFFERS_PER_SD: usize = 16;
/// Cap on unique inbox ports per drain pass. 16 covers any
/// realistic catalog (offers + subscriptions); excess ports are
/// silently skipped this tick and picked up next.
const MAX_UNIQUE_PORTS: usize = 16;
