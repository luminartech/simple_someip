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
use core::marker::PhantomData;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::pin::pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use heapless::index_map::FnvIndexMap;
use heapless::Vec as HVec;

use crate::protocol::sd::{
    Entry, EntryType, EventGroupEntry, Flags, Header as SdHeader, OptionType, Options,
    RebootFlag, SdHeaderView, TransportProtocol,
};
use crate::protocol::{Header as SomeIpHeader, MessageView};
use crate::runtime::{AsyncUdpSocket, Clock, SocketFactory};
use crate::traits::{PayloadWireFormat, WireFormat};

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
pub struct Client<P, S, C, F>
where
    P: PayloadWireFormat,
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

    /// Outbound subscriptions to peer services, re-sent every
    /// renewal tick.
    outbound_subscriptions: HVec<OutboundSubscription, DEFAULT_MAX_OUTBOUND_SUBS>,

    /// Subscription renewal cadence (default 1000 ms).
    renewal_interval_ms: u32,

    /// Accumulated `elapsed_ms` since the last renewal pass.
    renewal_accumulator_ms: u32,

    _phantom: PhantomData<P>,
}

impl<P, S, C, F> Client<P, S, C, F>
where
    P: PayloadWireFormat,
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
            outbound_subscriptions: HVec::new(),
            renewal_interval_ms: 1000,
            renewal_accumulator_ms: 0,
            _phantom: PhantomData,
        }
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
    /// # Errors
    /// Returns [`Error::CapacityExceeded`] if the offered-service
    /// table is full.
    pub fn offer_service(
        &mut self,
        service_id: u16,
        instance_id: u16,
        eventgroup_id: u16,
    ) -> Result<(), Error> {
        let key = OfferedKey { service_id, instance_id, eventgroup_id };
        if self.subscribers.contains_key(&key) {
            return Ok(());
        }
        self.subscribers
            .insert(key, HVec::new())
            .map_err(|_| Error::CapacityExceeded)?;
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
    pub fn tick<H: EventHandler<P>>(&mut self, elapsed_ms: u32, handler: &mut H) {
        let mut rx_buf = [0u8; 1500];
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // Discovery
        if let Some(slot) = self.discovery.as_ref()
            && let Poll::Ready(Ok((len, src))) = slot.socket.poll_recv_from(&mut cx, &mut rx_buf)
        {
            Self::dispatch_rx(
                &mut self.subscribers,
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
                    handler,
                    &rx_buf[..len],
                    src,
                    /* is_sd */ false,
                );
            }
        }

        // Tick TTLs on tracked subscribers.
        Self::tick_subscriber_ttls(&mut self.subscribers, elapsed_ms);

        // Renew outbound subscriptions on cadence.
        self.renewal_accumulator_ms = self.renewal_accumulator_ms.saturating_add(elapsed_ms);
        if self.renewal_accumulator_ms >= self.renewal_interval_ms {
            self.renewal_accumulator_ms = 0;
            self.renew_outbound_subscriptions();
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

    /// Parse a received datagram and either record subscribers
    /// (SD) or dispatch the event (unicast).
    fn dispatch_rx<H: EventHandler<P>>(
        subscribers: &mut FnvIndexMap<
            OfferedKey,
            HVec<SubscriberEndpoint, DEFAULT_MAX_SUBSCRIBERS>,
            DEFAULT_MAX_OFFERED_SERVICES,
        >,
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
            Self::process_sd_payload(subscribers, payload, src);
        } else {
            // Best-effort event dispatch. E2E checking is deferred
            // to Phase 5.5c — for now `e2e_status` is reported as
            // `Unchecked` (0).
            handler.on_event(
                hdr.message_id().service_id(),
                hdr.message_id().method_id(),
                payload,
                0,
            );
        }
    }

    /// Walk the entries in an SD payload, recording any `Subscribe`
    /// entry that targets one of our offered (service, instance,
    /// eventgroup) tuples.
    fn process_sd_payload(
        subscribers: &mut FnvIndexMap<
            OfferedKey,
            HVec<SubscriberEndpoint, DEFAULT_MAX_SUBSCRIBERS>,
            DEFAULT_MAX_OFFERED_SERVICES,
        >,
        payload: &[u8],
        _src: SocketAddrV4,
    ) {
        let Ok(sd_view) = SdHeaderView::parse(payload) else {
            return;
        };
        for entry in sd_view.entries() {
            let Ok(entry_type) = entry.entry_type() else {
                continue;
            };
            if entry_type != EntryType::Subscribe {
                continue;
            }
            let svc = entry.service_id();
            let inst = entry.instance_id();
            let eg = entry.event_group_id();
            let ttl_secs = entry.ttl();
            let key = OfferedKey { service_id: svc, instance_id: inst, eventgroup_id: eg };
            let Some(slot) = subscribers.get_mut(&key) else {
                // Not one of our offered tuples.
                continue;
            };
            // Find the first IPv4 endpoint option in this entry's
            // first-options run; that's the subscriber's receive
            // endpoint.
            let first_idx = entry.index_first_options_run() as usize;
            let first_count = entry.options_count().first_options_count as usize;
            let Some(endpoint) = sd_view
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
                .next()
            else {
                continue;
            };
            // Record / refresh subscriber.
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
pub trait EventHandler<P> {
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
