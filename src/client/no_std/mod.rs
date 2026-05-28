// The skeleton lands fields and types used by the upcoming `run` loop
// implementation. They are dead until that lands; suppress the warning
// at the module level rather than spraying it across declarations.
#![allow(dead_code)]

//! `no_std`, `no_alloc` SOME/IP client / server suitable for embedded
//! firmware that drives the run loop from an external tick.
//!
//! Unlike [`crate::client::Client`] — which depends on tokio's mpsc /
//! oneshot channels and `std::collections` and is therefore std-bound
//! — this module's [`Client`] holds all its state in
//! `heapless`-backed collections and exposes a direct method API
//! (no command-mpsc indirection). The run loop is a single
//! `pub async fn run` that the caller drives via any executor —
//! including the [`crate::executors::polled`] mini-executor.
//!
//! ## Generic over the runtime traits
//!
//! `Client<P, S, C, F>` is parameterised over the
//! [`runtime::AsyncUdpSocket`](crate::runtime::AsyncUdpSocket) /
//! [`runtime::Clock`](crate::runtime::Clock) /
//! [`runtime::SocketFactory`](crate::runtime::SocketFactory) trio.
//! Any consumer can plug in an implementation tailored to their
//! transport — lwIP, embassy-net, smoltcp, mock-for-test, etc.
//!
//! ## Bounded state
//!
//! All collections are sized at compile time via `heapless`. The
//! default capacities are sized for the iris-sensor catalog the
//! halo firmware uses; downstream projects with different needs
//! re-specify via the const generic parameters.

use core::future::Future;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::pin::pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use heapless::index_map::FnvIndexMap;
use heapless::Vec as HVec;

use crate::e2e::{check_profile5, check_profile5_with_header, E2EKey, Profile5Config, Profile5State};
use crate::protocol::sd::{
    Entry, EntryType, EventGroupEntry, Flags, Header as SdHeader, OptionType, Options,
    OptionsCount, RebootFlag, SdHeaderView, ServiceEntry, TransportProtocol, MULTICAST_IP,
    MULTICAST_PORT,
};
use crate::protocol::{Header as SomeIpHeader, MessageView};
use crate::runtime::{AsyncUdpSocket, Clock, SocketFactory};
use crate::traits::WireFormat;

/// SOME/IP service ID for SD messages (per spec).
const SD_SERVICE_ID: u16 = 0xFFFF;

// ---------------------------------------------------------------------------
// Helpers for synchronously driving async fns to completion
// ---------------------------------------------------------------------------
//
// All AsyncUdpSocket / SocketFactory impls intended for use with this
// Client are expected to be "ready immediately" — they wrap host
// callbacks that return synchronously. We drive each async call with
// a no-op waker and poll once; `Pending` is treated as an
// implementation error.

static NOOP_VTABLE: RawWakerVTable = RawWakerVTable::new(
    |_| RawWaker::new(core::ptr::null(), &NOOP_VTABLE),
    |_| {},
    |_| {},
    |_| {},
);

fn noop_waker() -> Waker {
    // SAFETY: all vtable entries are no-ops that ignore the data ptr.
    unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &NOOP_VTABLE)) }
}

#[inline]
fn poll_once<F: Future>(mut fut: core::pin::Pin<&mut F>) -> Option<F::Output> {
    let w = noop_waker();
    let mut cx = Context::from_waker(&w);
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(v) => Some(v),
        Poll::Pending => None,
    }
}

// ---------------------------------------------------------------------------
// Configuration constants — defaults sized for the halo / iris catalog.
// ---------------------------------------------------------------------------

/// Maximum unicast PCBs the client owns at once.
pub const DEFAULT_MAX_UNICAST_SOCKETS: usize = 4;
/// Maximum subscribers tracked per offered (service, eventgroup) tuple.
pub const DEFAULT_MAX_SUBSCRIBERS: usize = 4;
/// Maximum offered services we publish events on.
pub const DEFAULT_MAX_OFFERED_SERVICES: usize = 8;
/// Maximum outbound subscriptions tracked.
pub const DEFAULT_MAX_OUTBOUND_SUBS: usize = 8;
/// Maximum E2E-protected (service, method) tuples tracked.
pub const DEFAULT_MAX_E2E_ENTRIES: usize = 8;
/// Largest SOME/IP datagram processed; sized to fit `HWP1ScanCommand`.
pub const DEFAULT_RX_BUF: usize = 12 * 1024;

// ---------------------------------------------------------------------------
// Public error type
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub enum Error {
    /// Bind via the factory failed.
    BindFailed,
    /// Send via the underlying socket failed.
    SendFailed,
    /// A bounded heapless collection ran out of room.
    CapacityExceeded,
    /// Tried to operate on a service/socket that has not been opened.
    NotBound,
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

/// State for the SD multicast socket.
struct DiscoverySlot<S: AsyncUdpSocket> {
    socket: S,
    /// SD session counter for outbound SD messages.
    session_id: u16,
    /// `true` once the session counter has wrapped from 0xFFFF to 1.
    session_has_wrapped: bool,
}

/// State for a unicast socket.
struct UnicastSlot<S: AsyncUdpSocket> {
    socket: S,
    local_port: u16,
    /// Per-socket session counter for outbound unicast SOME/IP messages.
    session_id: u16,
}

/// A peer endpoint that has subscribed to one of our offered services.
#[derive(Clone, Copy, Debug)]
struct SubscriberEndpoint {
    addr: Ipv4Addr,
    port: u16,
    /// Subscription TTL countdown in milliseconds; subscription is
    /// dropped once this reaches 0.
    ttl_remaining_ms: u32,
}

/// Identifies an offered (service_id, instance_id, eventgroup_id) tuple.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
struct OfferedKey {
    service_id: u16,
    instance_id: u16,
    eventgroup_id: u16,
}

/// Service we offer on the wire. Re-announced via OfferService SD
/// on every renewal tick so peers can discover and subscribe to us.
#[derive(Clone, Copy, Debug)]
struct OfferedServiceConfig {
    service_id: u16,
    instance_id: u16,
    major_version: u8,
    /// SD entry TTL in seconds.
    ttl_secs: u32,
}

/// Tracks one outbound `Subscribe` we've sent to a peer that offers
/// a service we want to receive events from. Re-sent every renewal
/// tick until the entry is removed.
#[derive(Clone, Copy, Debug)]
struct OutboundSubscription {
    service_id: u16,
    instance_id: u16,
    eventgroup_id: u16,
    major_version: u8,
    ttl_secs: u32,
    /// Where to send the SD subscribe message (the peer's SD port).
    target: SocketAddrV4,
    /// The local port we want events delivered to.
    receive_port: u16,
}

/// E2E Profile 5 configuration + state for one (service, method)
/// tuple. Used to verify inbound notifications protected with
/// AUTOSAR E2E Profile 5.
#[derive(Clone, Debug)]
struct E2EEntry {
    key: E2EKey,
    config: Profile5Config,
    state: Profile5State,
    /// `true` to include the SOME/IP upper-header bytes in the CRC
    /// (the variant most AUTOSAR specs require). `false` for plain
    /// Profile 5.
    with_header: bool,
}

/// A service we want to consume. The Client periodically sends
/// `FindService` SD until a matching `OfferService` arrives; once
/// the peer's endpoint is known we begin sending periodic
/// `Subscribe` SD to that endpoint.
#[derive(Clone, Copy, Debug)]
struct DesiredService {
    service_id: u16,
    instance_id: u16,
    eventgroup_id: u16,
    major_version: u8,
    ttl_secs: u32,
    /// Local port we want events delivered to.
    receive_port: u16,
    /// `Some(SD endpoint)` once we have received a matching
    /// `OfferService` SD entry from a peer. The endpoint's port is
    /// the SD multicast port (30490).
    discovered: Option<SocketAddrV4>,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// `no_std` SOME/IP client/server.
///
/// Construct via [`Client::new`], then call [`bind_discovery`](Self::bind_discovery) and
/// [`bind_unicast`](Self::bind_unicast) to open the necessary sockets.
/// Register the services you offer with
/// [`offer_service`](Self::offer_service) and submit outbound
/// subscriptions via [`subscribe`](Self::subscribe). Finally call
/// [`run`](Self::run) (typically pinned in a static and driven by the
/// polled executor) to process inbound traffic and renew
/// subscriptions on a periodic cadence.
pub struct Client<S, C, F>
where
    S: AsyncUdpSocket,
    C: Clock,
    F: SocketFactory<Socket = S>,
{
    interface: Ipv4Addr,
    factory: F,
    clock: C,
    multicast_loopback: bool,

    discovery: Option<DiscoverySlot<S>>,
    unicast_sockets: HVec<UnicastSlot<S>, DEFAULT_MAX_UNICAST_SOCKETS>,

    /// Subscribers to our offered services, keyed by
    /// (service_id, instance_id, eventgroup_id).
    subscribers: FnvIndexMap<
        OfferedKey,
        HVec<SubscriberEndpoint, DEFAULT_MAX_SUBSCRIBERS>,
        DEFAULT_MAX_OFFERED_SERVICES,
    >,

    /// Services we announce via OfferService SD on each renewal
    /// tick. Deduplicated by (service_id, instance_id).
    offered_services: HVec<OfferedServiceConfig, DEFAULT_MAX_OFFERED_SERVICES>,

    /// Local UDP source port we offer services on — included in
    /// the IpV4Endpoint option of outbound OfferService SD entries.
    provided_port: u16,

    /// Outbound subscriptions to peer services, re-sent every
    /// renewal tick.
    outbound_subscriptions: HVec<OutboundSubscription, DEFAULT_MAX_OUTBOUND_SUBS>,

    /// Services we want to consume but have not yet discovered.
    /// Drives FindService SD on the renewal cadence until matching
    /// OfferService arrives, then begins the Subscribe cadence.
    desired_services: HVec<DesiredService, DEFAULT_MAX_OUTBOUND_SUBS>,

    /// E2E Profile 5 verification entries, keyed by
    /// (service_id, method_or_event_id).
    e2e_entries: HVec<E2EEntry, DEFAULT_MAX_E2E_ENTRIES>,

    /// Subscription renewal cadence (default 1000 ms).
    renewal_interval_ms: u32,

    /// Accumulated `elapsed_ms` since the last renewal pass.
    renewal_accumulator_ms: u32,
}

impl<S, C, F> Client<S, C, F>
where
    S: AsyncUdpSocket,
    C: Clock,
    F: SocketFactory<Socket = S>,
{
    /// Create a fresh client. No sockets are opened yet; call
    /// [`bind_discovery`](Self::bind_discovery) and
    /// [`bind_unicast`](Self::bind_unicast) afterwards.
    pub fn new(factory: F, clock: C, interface: Ipv4Addr) -> Self {
        Self {
            interface,
            factory,
            clock,
            multicast_loopback: false,
            discovery: None,
            unicast_sockets: HVec::new(),
            subscribers: FnvIndexMap::new(),
            offered_services: HVec::new(),
            provided_port: 0,
            outbound_subscriptions: HVec::new(),
            desired_services: HVec::new(),
            e2e_entries: HVec::new(),
            renewal_interval_ms: 1000,
            // Trigger an OfferService send on the very first tick
            // so peers see us promptly after boot.
            renewal_accumulator_ms: 0,
        }
    }

    /// Set the local source UDP port for events the application
    /// publishes. This port is also advertised in the IpV4Endpoint
    /// option of outbound OfferService SD entries — peers send
    /// `Subscribe` SD messages here.
    #[must_use]
    pub fn with_provided_port(mut self, port: u16) -> Self {
        self.provided_port = port;
        self
    }

    /// Enable loopback on the SD multicast socket so packets sent
    /// here are also received here. Useful for same-host test rigs.
    /// Must be called before [`bind_discovery`](Self::bind_discovery).
    pub fn with_multicast_loopback(mut self, enable: bool) -> Self {
        self.multicast_loopback = enable;
        self
    }

    /// Override the default subscription renewal cadence.
    pub fn with_renewal_interval_ms(mut self, ms: u32) -> Self {
        self.renewal_interval_ms = if ms == 0 { 1000 } else { ms };
        self
    }

    /// Bind the SD multicast socket on port 30490 and join the
    /// SOME/IP-SD multicast group. Idempotent — second call is a
    /// no-op once bound.
    ///
    /// # Errors
    /// Returns [`Error::BindFailed`] if the factory's
    /// `bind_discovery` returns an error.
    pub async fn bind_discovery(&mut self) -> Result<(), Error> {
        if self.discovery.is_some() {
            return Ok(());
        }
        let socket = self
            .factory
            .bind_discovery(self.interface, self.multicast_loopback)
            .await
            .map_err(|_| Error::BindFailed)?;
        self.discovery = Some(DiscoverySlot {
            socket,
            session_id: 1,
            session_has_wrapped: false,
        });
        Ok(())
    }

    /// Bind a unicast PCB on `port` (0 = ephemeral). Returns the
    /// actually-bound port. Idempotent — already-bound ports are
    /// returned without re-binding.
    ///
    /// # Errors
    /// Returns [`Error::BindFailed`] or
    /// [`Error::CapacityExceeded`].
    pub async fn bind_unicast(&mut self, port: u16) -> Result<u16, Error> {
        if port != 0 {
            for slot in &self.unicast_sockets {
                if slot.local_port == port {
                    return Ok(port);
                }
            }
        }
        let (socket, bound_port) = self
            .factory
            .bind_unicast(self.interface, port)
            .await
            .map_err(|_| Error::BindFailed)?;
        self.unicast_sockets
            .push(UnicastSlot {
                socket,
                local_port: bound_port,
                session_id: 1,
            })
            .map_err(|_| Error::CapacityExceeded)?;
        Ok(bound_port)
    }

    /// Register that we offer a (service, instance, eventgroup)
    /// tuple. Subscribers landing on this tuple are recorded and
    /// counted toward [`publish`](Self::publish) routing.
    ///
    /// `major_version` and `ttl_secs` are used when announcing this
    /// service via OfferService SD entries on the renewal cadence.
    ///
    /// # Errors
    /// Returns [`Error::CapacityExceeded`] if the offered-service
    /// table is full.
    pub fn offer_service(
        &mut self,
        service_id: u16,
        instance_id: u16,
        eventgroup_id: u16,
        major_version: u8,
        ttl_secs: u32,
    ) -> Result<(), Error> {
        let key = OfferedKey { service_id, instance_id, eventgroup_id };
        if !self.subscribers.contains_key(&key) {
            self.subscribers
                .insert(key, HVec::new())
                .map_err(|_| Error::CapacityExceeded)?;
        }
        // Dedup on (service_id, instance_id) — one OfferService SD
        // entry covers all eventgroups of the same instance.
        if !self
            .offered_services
            .iter()
            .any(|o| o.service_id == service_id && o.instance_id == instance_id)
        {
            self.offered_services
                .push(OfferedServiceConfig {
                    service_id,
                    instance_id,
                    major_version,
                    ttl_secs,
                })
                .map_err(|_| Error::CapacityExceeded)?;
        }
        Ok(())
    }

    /// Return the number of currently-tracked subscribers for the
    /// given offered tuple.
    #[must_use]
    pub fn subscriber_count(&self, service_id: u16, instance_id: u16, eventgroup_id: u16) -> usize {
        self.subscribers
            .get(&OfferedKey { service_id, instance_id, eventgroup_id })
            .map_or(0, |v| v.len())
    }

    /// Register E2E Profile 5 verification for a
    /// (service_id, method_or_event_id) tuple. Inbound unicast
    /// notifications matching the key will be checked, their E2E
    /// header stripped, and the resulting status delivered to the
    /// [`EventHandler::on_event`] callback's `e2e_status` parameter
    /// (values match [`E2ECheckStatus::to_return_code`]).
    ///
    /// `data_length` is the expected payload size in bytes
    /// **excluding** the 3-byte E2E header.
    ///
    /// Set `with_header = true` for the AUTOSAR variant that
    /// includes the SOME/IP upper-header (8 bytes) in the CRC; this
    /// is the configuration most production catalogs use.
    ///
    /// # Errors
    /// Returns [`Error::CapacityExceeded`] if the E2E table is
    /// full.
    pub fn configure_e2e_profile5(
        &mut self,
        service_id: u16,
        method_or_event_id: u16,
        data_id: u16,
        data_length: u16,
        max_delta_counter: u8,
        with_header: bool,
    ) -> Result<(), Error> {
        let key = E2EKey::new(service_id, method_or_event_id);
        if let Some(existing) = self.e2e_entries.iter_mut().find(|e| e.key == key) {
            existing.config = Profile5Config::new(data_id, data_length, max_delta_counter);
            existing.state = Profile5State::new();
            existing.with_header = with_header;
            return Ok(());
        }
        self.e2e_entries
            .push(E2EEntry {
                key,
                config: Profile5Config::new(data_id, data_length, max_delta_counter),
                state: Profile5State::new(),
                with_header,
            })
            .map_err(|_| Error::CapacityExceeded)?;
        Ok(())
    }

    /// Register a desired service we want to consume.
    ///
    /// The Client will periodically send `FindService` SD until a
    /// matching `OfferService` arrives from a peer; it then begins
    /// sending periodic `SubscribeEventGroup` SD to that peer.
    /// Events arriving on `receive_port` get dispatched through the
    /// [`EventHandler`] passed to [`tick`](Self::tick).
    ///
    /// # Errors
    /// Returns [`Error::CapacityExceeded`] if the desired-service
    /// table is full.
    pub fn want_service(
        &mut self,
        service_id: u16,
        instance_id: u16,
        eventgroup_id: u16,
        major_version: u8,
        ttl_secs: u32,
        receive_port: u16,
    ) -> Result<(), Error> {
        // Dedup on (service, instance, eventgroup).
        if self.desired_services.iter().any(|d| {
            d.service_id == service_id
                && d.instance_id == instance_id
                && d.eventgroup_id == eventgroup_id
        }) {
            return Ok(());
        }
        self.desired_services
            .push(DesiredService {
                service_id,
                instance_id,
                eventgroup_id,
                major_version,
                ttl_secs,
                receive_port,
                discovered: None,
            })
            .map_err(|_| Error::CapacityExceeded)?;
        Ok(())
    }

    /// Submit an outbound subscription to a peer-offered service.
    ///
    /// Sends a Subscribe SD entry to `target` (the peer's SD port)
    /// asking the peer to deliver events for
    /// `(service_id, instance_id, eventgroup_id)` to our
    /// `receive_port`. The subscription is recorded and re-sent on
    /// every renewal tick.
    ///
    /// # Errors
    /// Returns [`Error::NotBound`] if the discovery socket has not
    /// been bound; [`Error::SendFailed`] on encoding or transport
    /// errors; [`Error::CapacityExceeded`] if the outbound-sub
    /// table is full.
    pub fn subscribe(
        &mut self,
        service_id: u16,
        instance_id: u16,
        eventgroup_id: u16,
        major_version: u8,
        ttl_secs: u32,
        target: SocketAddrV4,
        receive_port: u16,
    ) -> Result<(), Error> {
        let sub = OutboundSubscription {
            service_id,
            instance_id,
            eventgroup_id,
            major_version,
            ttl_secs,
            target,
            receive_port,
        };
        Self::send_subscribe_sd(
            self.discovery.as_mut().ok_or(Error::NotBound)?,
            self.interface,
            &sub,
        )?;
        // Refresh if we already have this exact subscription.
        if let Some(existing) = self
            .outbound_subscriptions
            .iter_mut()
            .find(|s| {
                s.service_id == service_id
                    && s.instance_id == instance_id
                    && s.eventgroup_id == eventgroup_id
                    && s.target == target
            })
        {
            *existing = sub;
        } else {
            self.outbound_subscriptions
                .push(sub)
                .map_err(|_| Error::CapacityExceeded)?;
        }
        Ok(())
    }

    /// Publish a notification on a service we offer.
    ///
    /// Looks up tracked subscribers for
    /// `(service_id, instance_id, eventgroup_id)` and sends the
    /// notification to each via the unicast socket bound on
    /// `source_port`. Returns the number of subscribers the message
    /// was delivered to.
    ///
    /// # Errors
    /// Returns [`Error::NotBound`] if either the offered tuple has
    /// not been registered via [`offer_service`](Self::offer_service)
    /// or no unicast socket is bound on `source_port`.
    /// [`Error::SendFailed`] on encoding errors.
    pub fn publish(
        &mut self,
        service_id: u16,
        instance_id: u16,
        eventgroup_id: u16,
        method_id: u16,
        payload: &[u8],
        source_port: u16,
    ) -> Result<usize, Error> {
        let key = OfferedKey {
            service_id,
            instance_id,
            eventgroup_id,
        };
        // Snapshot subscriber endpoints so we don't hold a borrow of
        // self.subscribers across the &mut borrow on unicast_sockets.
        let mut targets: HVec<SocketAddrV4, DEFAULT_MAX_SUBSCRIBERS> = HVec::new();
        let Some(slot) = self.subscribers.get(&key) else {
            return Err(Error::NotBound);
        };
        if slot.is_empty() {
            return Ok(0);
        }
        for sub in slot {
            let _ = targets.push(SocketAddrV4::new(sub.addr, sub.port));
        }

        let Some(sock_slot) = self
            .unicast_sockets
            .iter_mut()
            .find(|s| s.local_port == source_port)
        else {
            return Err(Error::NotBound);
        };

        let mut sent = 0usize;
        for target in targets.iter() {
            if Self::send_event(sock_slot, *target, service_id, method_id, payload).is_ok() {
                sent += 1;
            }
        }
        Ok(sent)
    }

    /// Build + send a Subscribe SD message via the discovery socket.
    fn send_subscribe_sd(
        discovery: &mut DiscoverySlot<S>,
        local_addr: Ipv4Addr,
        sub: &OutboundSubscription,
    ) -> Result<(), Error> {
        // Build SD payload: one SubscribeEventGroup entry + one
        // IpV4Endpoint option pointing at our receive endpoint.
        let entry = Entry::SubscribeEventGroup(EventGroupEntry::new(
            sub.service_id,
            sub.instance_id,
            sub.major_version,
            sub.ttl_secs,
            sub.eventgroup_id,
        ));
        let option = Options::IpV4Endpoint {
            ip: local_addr,
            protocol: TransportProtocol::Udp,
            port: sub.receive_port,
        };
        let reboot = if discovery.session_has_wrapped {
            RebootFlag::Continuous
        } else {
            RebootFlag::RecentlyRebooted
        };
        let entries = [entry];
        let options = [option];
        let sd_hdr = SdHeader::new(Flags::new_sd(reboot), &entries, &options);

        // Encode SD payload first.
        let mut sd_buf = [0u8; 128];
        let sd_len = sd_hdr
            .encode_to_slice(&mut sd_buf)
            .map_err(|_| Error::SendFailed)?;

        // Wrap in a SOME/IP SD header.
        let request_id = u32::from(discovery.session_id);
        let someip_hdr = SomeIpHeader::new_sd(request_id, sd_len);

        let mut buf = [0u8; 256];
        let hdr_len = someip_hdr
            .encode_to_slice(&mut buf)
            .map_err(|_| Error::SendFailed)?;
        if hdr_len + sd_len > buf.len() {
            return Err(Error::SendFailed);
        }
        buf[hdr_len..hdr_len + sd_len].copy_from_slice(&sd_buf[..sd_len]);
        let total = hdr_len + sd_len;

        // Drive the async send synchronously.
        let fut = discovery.socket.send_to(&buf[..total], sub.target);
        let mut fut = pin!(fut);
        match poll_once(fut.as_mut()) {
            Some(Ok(())) => {}
            Some(Err(_)) | None => return Err(Error::SendFailed),
        }

        Self::advance_session(&mut discovery.session_id, &mut discovery.session_has_wrapped);
        Ok(())
    }

    /// Build + send a single Notification SOME/IP message via a
    /// unicast socket. Advances that socket's session counter on
    /// success.
    fn send_event(
        slot: &mut UnicastSlot<S>,
        target: SocketAddrV4,
        service_id: u16,
        method_id: u16,
        payload: &[u8],
    ) -> Result<(), Error> {
        let request_id = u32::from(slot.session_id);
        let header = SomeIpHeader::new_event(service_id, method_id, request_id, 1, 1, payload.len());

        let mut buf = [0u8; 1500];
        let hdr_len = header
            .encode_to_slice(&mut buf)
            .map_err(|_| Error::SendFailed)?;
        if hdr_len + payload.len() > buf.len() {
            return Err(Error::SendFailed);
        }
        buf[hdr_len..hdr_len + payload.len()].copy_from_slice(payload);
        let total = hdr_len + payload.len();

        let fut = slot.socket.send_to(&buf[..total], target);
        let mut fut = pin!(fut);
        match poll_once(fut.as_mut()) {
            Some(Ok(())) => {}
            Some(Err(_)) | None => return Err(Error::SendFailed),
        }

        let mut wrapped_dummy = false;
        Self::advance_session(&mut slot.session_id, &mut wrapped_dummy);
        Ok(())
    }

    fn advance_session(session_id: &mut u16, wrapped: &mut bool) {
        if *session_id == u16::MAX {
            *session_id = 1;
            *wrapped = true;
        } else {
            *session_id += 1;
        }
    }

    /// Drive the client/server forward by one step.
    ///
    /// Drains a single received datagram from each bound socket
    /// (discovery + each unicast), decrements subscriber TTLs by
    /// `elapsed_ms`, and — if a renewal interval has passed —
    /// re-sends every tracked outbound subscription.
    ///
    /// Call periodically from the host tick. `elapsed_ms` should be
    /// the milliseconds elapsed since the last `tick` call.
    pub fn tick<H: EventHandler>(&mut self, elapsed_ms: u32, handler: &mut H) {
        let mut rx_buf = [0u8; 1500];
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // Discovery
        if let Some(slot) = self.discovery.as_ref()
            && let Poll::Ready(Ok((len, src))) = slot.socket.poll_recv_from(&mut cx, &mut rx_buf)
        {
            Self::dispatch_rx(
                &mut self.subscribers,
                &mut self.desired_services,
                &mut self.e2e_entries,
                handler,
                &rx_buf[..len],
                src,
                /* is_sd */ true,
            );
        }

        // Each unicast (one packet per socket per tick).
        for slot in self.unicast_sockets.iter() {
            if let Poll::Ready(Ok((len, src))) = slot.socket.poll_recv_from(&mut cx, &mut rx_buf) {
                Self::dispatch_rx(
                    &mut self.subscribers,
                    &mut self.desired_services,
                    &mut self.e2e_entries,
                    handler,
                    &rx_buf[..len],
                    src,
                    /* is_sd */ false,
                );
            }
        }

        // Tick TTLs on tracked subscribers.
        Self::tick_subscriber_ttls(&mut self.subscribers, elapsed_ms);

        // Renew outbound subscriptions, re-announce offered
        // services, and probe undiscovered desired services on
        // cadence.
        self.renewal_accumulator_ms = self.renewal_accumulator_ms.saturating_add(elapsed_ms);
        if self.renewal_accumulator_ms >= self.renewal_interval_ms {
            self.renewal_accumulator_ms = 0;
            self.renew_outbound_subscriptions();
            self.renew_offered_services();
            self.renew_discovery_probes();
        }
    }

    /// Send `FindService` for each desired service we haven't
    /// discovered yet, and `Subscribe` for those we have.
    fn renew_discovery_probes(&mut self) {
        let Some(discovery) = self.discovery.as_mut() else {
            return;
        };
        let interface = self.interface;
        for desire in self.desired_services.iter() {
            match desire.discovered {
                None => {
                    let _ = Self::send_findservice_sd(
                        discovery,
                        desire.service_id,
                        desire.instance_id,
                        desire.major_version,
                        desire.ttl_secs,
                    );
                }
                Some(target) => {
                    let sub = OutboundSubscription {
                        service_id: desire.service_id,
                        instance_id: desire.instance_id,
                        eventgroup_id: desire.eventgroup_id,
                        major_version: desire.major_version,
                        ttl_secs: desire.ttl_secs,
                        target,
                        receive_port: desire.receive_port,
                    };
                    let _ = Self::send_subscribe_sd(discovery, interface, &sub);
                }
            }
        }
    }

    fn renew_outbound_subscriptions(&mut self) {
        let Some(discovery) = self.discovery.as_mut() else {
            return;
        };
        for sub in self.outbound_subscriptions.iter() {
            let _ = Self::send_subscribe_sd(discovery, self.interface, sub);
        }
    }

    fn renew_offered_services(&mut self) {
        let Some(discovery) = self.discovery.as_mut() else {
            return;
        };
        let provided_port = self.provided_port;
        let interface = self.interface;
        for offer in self.offered_services.iter() {
            let _ = Self::send_offer_service_sd(discovery, interface, provided_port, offer);
        }
    }

    /// Build + send a FindService SD message asking peers to
    /// announce themselves for `(service_id, instance_id)`. Sent
    /// to the SD multicast group.
    fn send_findservice_sd(
        discovery: &mut DiscoverySlot<S>,
        service_id: u16,
        instance_id: u16,
        major_version: u8,
        ttl_secs: u32,
    ) -> Result<(), Error> {
        // FindService entry: 0 options. Use the spec's wildcards
        // except where we have a concrete instance / version.
        let entry = Entry::FindService(ServiceEntry {
            index_first_options_run: 0,
            index_second_options_run: 0,
            options_count: OptionsCount::new(0, 0),
            service_id,
            instance_id,
            major_version,
            ttl: ttl_secs,
            minor_version: 0xFFFF_FFFF,
        });
        let reboot = if discovery.session_has_wrapped {
            RebootFlag::Continuous
        } else {
            RebootFlag::RecentlyRebooted
        };
        let entries = [entry];
        let options: [Options; 0] = [];
        let sd_hdr = SdHeader::new(Flags::new_sd(reboot), &entries, &options);

        let mut sd_buf = [0u8; 64];
        let sd_len = sd_hdr
            .encode_to_slice(&mut sd_buf)
            .map_err(|_| Error::SendFailed)?;

        let request_id = u32::from(discovery.session_id);
        let someip_hdr = SomeIpHeader::new_sd(request_id, sd_len);

        let mut buf = [0u8; 128];
        let hdr_len = someip_hdr
            .encode_to_slice(&mut buf)
            .map_err(|_| Error::SendFailed)?;
        if hdr_len + sd_len > buf.len() {
            return Err(Error::SendFailed);
        }
        buf[hdr_len..hdr_len + sd_len].copy_from_slice(&sd_buf[..sd_len]);
        let total = hdr_len + sd_len;

        let target = SocketAddrV4::new(MULTICAST_IP, MULTICAST_PORT);
        let fut = discovery.socket.send_to(&buf[..total], target);
        let mut fut = pin!(fut);
        match poll_once(fut.as_mut()) {
            Some(Ok(())) => {}
            Some(Err(_)) | None => return Err(Error::SendFailed),
        }
        Self::advance_session(&mut discovery.session_id, &mut discovery.session_has_wrapped);
        Ok(())
    }

    /// Build + send an OfferService SD message announcing one
    /// (service_id, instance_id) to the SD multicast group.
    fn send_offer_service_sd(
        discovery: &mut DiscoverySlot<S>,
        local_addr: Ipv4Addr,
        provided_port: u16,
        offer: &OfferedServiceConfig,
    ) -> Result<(), Error> {
        let entry = Entry::OfferService(ServiceEntry {
            index_first_options_run: 0,
            index_second_options_run: 0,
            options_count: OptionsCount::new(1, 0),
            service_id: offer.service_id,
            instance_id: offer.instance_id,
            major_version: offer.major_version,
            ttl: offer.ttl_secs,
            minor_version: 0,
        });
        let option = Options::IpV4Endpoint {
            ip: local_addr,
            protocol: TransportProtocol::Udp,
            port: provided_port,
        };
        let reboot = if discovery.session_has_wrapped {
            RebootFlag::Continuous
        } else {
            RebootFlag::RecentlyRebooted
        };
        let entries = [entry];
        let options = [option];
        let sd_hdr = SdHeader::new(Flags::new_sd(reboot), &entries, &options);

        let mut sd_buf = [0u8; 128];
        let sd_len = sd_hdr
            .encode_to_slice(&mut sd_buf)
            .map_err(|_| Error::SendFailed)?;

        let request_id = u32::from(discovery.session_id);
        let someip_hdr = SomeIpHeader::new_sd(request_id, sd_len);

        let mut buf = [0u8; 256];
        let hdr_len = someip_hdr
            .encode_to_slice(&mut buf)
            .map_err(|_| Error::SendFailed)?;
        if hdr_len + sd_len > buf.len() {
            return Err(Error::SendFailed);
        }
        buf[hdr_len..hdr_len + sd_len].copy_from_slice(&sd_buf[..sd_len]);
        let total = hdr_len + sd_len;

        let target = SocketAddrV4::new(MULTICAST_IP, MULTICAST_PORT);
        let fut = discovery.socket.send_to(&buf[..total], target);
        let mut fut = pin!(fut);
        match poll_once(fut.as_mut()) {
            Some(Ok(())) => {}
            Some(Err(_)) | None => return Err(Error::SendFailed),
        }

        Self::advance_session(&mut discovery.session_id, &mut discovery.session_has_wrapped);
        Ok(())
    }

    /// Parse a received datagram and either process SD entries or
    /// dispatch the unicast event (applying E2E Profile 5 checking
    /// when configured for the matching key).
    fn dispatch_rx<H: EventHandler>(
        subscribers: &mut FnvIndexMap<
            OfferedKey,
            HVec<SubscriberEndpoint, DEFAULT_MAX_SUBSCRIBERS>,
            DEFAULT_MAX_OFFERED_SERVICES,
        >,
        desired_services: &mut HVec<DesiredService, DEFAULT_MAX_OUTBOUND_SUBS>,
        e2e_entries: &mut HVec<E2EEntry, DEFAULT_MAX_E2E_ENTRIES>,
        handler: &mut H,
        bytes: &[u8],
        src: SocketAddrV4,
        is_sd_socket: bool,
    ) {
        let Ok(msg_view) = MessageView::parse(bytes) else {
            handler.on_error(1 /* parse */, 0);
            return;
        };
        let hdr = msg_view.header();
        let payload = msg_view.payload_bytes();

        let is_sd_message = is_sd_socket
            && (hdr.message_id().service_id() == SD_SERVICE_ID
                || hdr.message_id().method_id() == 0x8100);

        if is_sd_message {
            Self::process_sd_payload(subscribers, desired_services, handler, payload, src);
            return;
        }

        // Look up E2E config; if present, run the check and
        // forward the stripped payload + status to the handler.
        let key = E2EKey::from_message_id(hdr.message_id());
        let (e2e_status, effective_payload) =
            if let Some(entry) = e2e_entries.iter_mut().find(|e| e.key == key) {
                let result = if entry.with_header {
                    // SOME/IP "upper header" is bytes 8..16 of the
                    // message: request_id(4) + proto_ver(1) + iface_ver(1)
                    // + msg_type(1) + return_code(1). Safe to slice
                    // because MessageView::parse already validated
                    // bytes.len() >= 16.
                    let mut upper_header = [0u8; 8];
                    upper_header.copy_from_slice(&bytes[8..16]);
                    check_profile5_with_header(
                        &entry.config,
                        &mut entry.state,
                        payload,
                        upper_header,
                    )
                } else {
                    check_profile5(&entry.config, &mut entry.state, payload)
                };
                let stripped = result.payload.unwrap_or(payload);
                (result.status.to_return_code(), stripped)
            } else {
                (0 /* Unchecked */, payload)
            };

        handler.on_event(
            hdr.message_id().service_id(),
            hdr.message_id().method_id(),
            effective_payload,
            e2e_status,
        );
    }

    /// Walk the entries in an SD payload, processing the ones that
    /// affect our state:
    ///
    /// * `Subscribe` entries targeting an offered tuple → record /
    ///   refresh the subscriber endpoint.
    /// * `OfferService` entries targeting a desired service →
    ///   record the peer's endpoint so the next renewal tick sends
    ///   a `SubscribeEventGroup` to it.
    fn process_sd_payload<H: EventHandler>(
        subscribers: &mut FnvIndexMap<
            OfferedKey,
            HVec<SubscriberEndpoint, DEFAULT_MAX_SUBSCRIBERS>,
            DEFAULT_MAX_OFFERED_SERVICES,
        >,
        desired_services: &mut HVec<DesiredService, DEFAULT_MAX_OUTBOUND_SUBS>,
        handler: &mut H,
        payload: &[u8],
        src: SocketAddrV4,
    ) {
        let Ok(sd_view) = SdHeaderView::parse(payload) else {
            return;
        };
        for entry in sd_view.entries() {
            let Ok(entry_type) = entry.entry_type() else {
                continue;
            };

            let first_idx = entry.index_first_options_run() as usize;
            let first_count = entry.options_count().first_options_count as usize;
            let ipv4_endpoint = sd_view
                .options()
                .skip(first_idx)
                .take(first_count)
                .filter_map(|opt| {
                    if matches!(opt.option_type(), Ok(OptionType::IpV4Endpoint)) {
                        opt.as_ipv4().ok().map(|(ip, _proto, port)| (ip, port))
                    } else {
                        None
                    }
                })
                .next();

            match entry_type {
                EntryType::Subscribe => {
                    let svc = entry.service_id();
                    let inst = entry.instance_id();
                    let eg = entry.event_group_id();
                    let ttl_secs = entry.ttl();
                    let key = OfferedKey {
                        service_id: svc,
                        instance_id: inst,
                        eventgroup_id: eg,
                    };
                    let Some(slot) = subscribers.get_mut(&key) else {
                        continue;
                    };
                    let Some(endpoint) = ipv4_endpoint else {
                        continue;
                    };
                    let ttl_ms = ttl_secs.saturating_mul(1000);
                    if let Some(existing) = slot
                        .iter_mut()
                        .find(|s| s.addr == endpoint.0 && s.port == endpoint.1)
                    {
                        existing.ttl_remaining_ms = ttl_ms;
                    } else {
                        let _ = slot.push(SubscriberEndpoint {
                            addr: endpoint.0,
                            port: endpoint.1,
                            ttl_remaining_ms: ttl_ms,
                        });
                    }
                }
                EntryType::OfferService => {
                    let svc = entry.service_id();
                    let inst = entry.instance_id();
                    // Find any desired service that wants this svc+inst.
                    let mut matched = false;
                    for desire in desired_services.iter_mut() {
                        if desire.service_id == svc && desire.instance_id == inst {
                            // The peer's SD endpoint is the source of
                            // this multicast message — they listen
                            // for our Subscribe there. The
                            // IpV4Endpoint option (when present)
                            // gives their unicast-event endpoint;
                            // we don't need that here (the host
                            // sends events back to receive_port we
                            // advertised) but we honour it if
                            // present to use that IP for Subscribe.
                            let peer_ip = ipv4_endpoint
                                .map(|(ip, _)| ip)
                                .unwrap_or_else(|| *src.ip());
                            desire.discovered = Some(SocketAddrV4::new(
                                peer_ip,
                                MULTICAST_PORT,
                            ));
                            matched = true;
                        }
                    }
                    if matched
                        && let Some((ep_ip, ep_port)) = ipv4_endpoint
                    {
                        handler.on_service_discovered(
                            svc,
                            inst,
                            ep_ip.to_bits(),
                            ep_port,
                        );
                    }
                }
                _ => {}
            }
        }
    }

    fn tick_subscriber_ttls(
        subscribers: &mut FnvIndexMap<
            OfferedKey,
            HVec<SubscriberEndpoint, DEFAULT_MAX_SUBSCRIBERS>,
            DEFAULT_MAX_OFFERED_SERVICES,
        >,
        elapsed_ms: u32,
    ) {
        for (_key, slot) in subscribers.iter_mut() {
            // Decrement and drop expired in-place via swap_remove.
            let mut i = 0;
            while i < slot.len() {
                slot[i].ttl_remaining_ms = slot[i].ttl_remaining_ms.saturating_sub(elapsed_ms);
                if slot[i].ttl_remaining_ms == 0 {
                    let _ = slot.swap_remove(i);
                } else {
                    i += 1;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Event handler trait
// ---------------------------------------------------------------------------

/// Receives events parsed by [`Client::tick`].
///
/// All methods are sync — implementations must not block the tick
/// call. Heavy processing should be queued to the application's
/// own task.
pub trait EventHandler {
    // Method signatures pass raw service_id / method_id / payload — no
    // type-erased generic per message type, matching the no_std
    // client's wire-level focus.
    /// Called for every received SOME/IP event matching a service
    /// we are subscribed to. `payload` is the application payload
    /// (E2E header already stripped when applicable). The reference
    /// is valid only for the duration of the call.
    fn on_event(&mut self, service_id: u16, method_id: u16, payload: &[u8], _e2e_status: u8) {
        let _ = (service_id, method_id, payload);
    }

    /// Called when a peer first announces a service we discovered
    /// via SD.
    fn on_service_discovered(
        &mut self,
        service_id: u16,
        instance_id: u16,
        endpoint_addr: u32,
        endpoint_port: u16,
    ) {
        let _ = (service_id, instance_id, endpoint_addr, endpoint_port);
    }

    /// Called on internal errors.
    fn on_error(&mut self, code: u32, context: u32) {
        let _ = (code, context);
    }
}
