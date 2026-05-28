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

use core::marker::PhantomData;
use core::net::{Ipv4Addr, SocketAddrV4};

use heapless::index_map::FnvIndexMap;
use heapless::Vec as HVec;

use crate::runtime::{AsyncUdpSocket, Clock, SocketFactory};
use crate::traits::PayloadWireFormat;

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
type OfferedKey = u32;

#[inline]
const fn offered_key(service_id: u16, instance_id: u16, eventgroup_id: u16) -> OfferedKey {
    // Pack the three u16s into a u32 — eventgroup_id in the upper 16
    // bits, service_id and instance_id share the lower 16 bits via
    // XOR. Acceptable because (service, instance) combinations are
    // limited and we just need a stable hash key.
    ((eventgroup_id as u32) << 16) | ((service_id as u32) ^ (instance_id as u32))
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

    /// Subscription renewal cadence (default 1000 ms).
    renewal_interval_ms: u32,

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
            renewal_interval_ms: 1000,
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
        let key = offered_key(service_id, instance_id, eventgroup_id);
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
            .get(&offered_key(service_id, instance_id, eventgroup_id))
            .map_or(0, |v| v.len())
    }

    /// Run the client/server loop until it errors or the future is
    /// dropped. Drives the SD subscription renewal cadence, parses
    /// inbound traffic, records subscribers, and dispatches received
    /// events to `handler`.
    ///
    /// Intended to be pinned in a `static` and polled by the
    /// [`crate::executors::polled`] executor from the host tick.
    pub async fn run<H: EventHandler<P>>(&mut self, _handler: &mut H) {
        // TODO(Phase 6 follow-up): implement the select! loop:
        //   - discovery_socket.poll_recv_from
        //   - each unicast_socket.poll_recv_from
        //   - clock.sleep_until(next_renewal)
        // For now, await forever so callers can wire up the
        // pinned-future story and bring the rest of the integration
        // online before this body lands.
        loop {
            self.clock
                .sleep_until(self.clock.now() + core::time::Duration::from_secs(60))
                .await;
        }
    }
}

// ---------------------------------------------------------------------------
// Event handler trait
// ---------------------------------------------------------------------------

/// Receives events parsed by [`Client::run`].
///
/// All methods are sync — implementations must not block the run
/// loop. Heavy processing should be queued to the application's
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
