use core::future;
use core::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use core::task::Poll;
use futures_util::{FutureExt, pin_mut, select_biased};
use heapless::{Deque, index_map::FnvIndexMap};
#[cfg(all(test, feature = "client-tokio"))]
use std::sync::{Arc, Mutex};
use crate::log::{debug, error, info, trace, warn};

#[cfg(all(test, feature = "client-tokio"))]
use crate::e2e::E2ERegistry;
#[cfg(all(test, feature = "client-tokio"))]
use crate::tokio_transport::{TokioChannels, TokioSpawner, TokioTimer, TokioTransport};
use crate::{
    Timer,
    client::{
        ClientUpdate, DiscoveryMessage,
        service_registry::{ServiceEndpointInfo, ServiceInstanceId, ServiceRegistry},
        session::{SessionTracker, SessionVerdict, TransportKind},
        socket_manager::{ReceivedMessage, SocketManager},
    },
    protocol::{self, Message},
    traits::PayloadWireFormat,
    transport::{ChannelFactory, E2ERegistryHandle, MpscRecv, OneshotSend, UnboundedSend},
};

use super::error::Error;

/// Max depth of the internal control-message queue. Each entry is one
/// in-flight `ControlMessage`. Must be generous enough to absorb bursts
/// from `Client` callers between event-loop ticks.
const REQUEST_QUEUE_CAP: usize = 32;

/// Max number of outstanding unicast request-response pairs. Each entry is
/// a `request_id` awaiting a reply. Must be a power of two.
const PENDING_RESPONSES_CAP: usize = 64;

/// Max number of bound unicast sockets tracked by port. Must be a power of
/// two.
const UNICAST_SOCKETS_CAP: usize = 8;

pub enum ControlMessage<P: PayloadWireFormat + 'static, C: ChannelFactory> {
    SetInterface(Ipv4Addr, C::OneshotSender<Result<(), Error>>),
    BindDiscovery(C::OneshotSender<Result<(), Error>>),
    UnbindDiscovery(C::OneshotSender<Result<(), Error>>),
    SendSD(
        SocketAddrV4,
        P::SdHeader,
        C::OneshotSender<Result<(), Error>>,
    ),
    AddEndpoint(
        u16,
        u16,
        SocketAddrV4,
        u16,
        C::OneshotSender<Result<(), Error>>,
    ),
    RemoveEndpoint(u16, u16, C::OneshotSender<Result<(), Error>>),
    SendToService {
        service_id: u16,
        instance_id: u16,
        message: Message<P>,
        /// Fires when the UDP send completes (or errors on lookup/bind).
        send_complete: C::OneshotSender<Result<(), Error>>,
        /// Fires when a matching unicast response arrives.
        response: C::OneshotSender<Result<P, Error>>,
    },
    Subscribe {
        service_id: u16,
        instance_id: u16,
        major_version: u8,
        ttl: u32,
        event_group_id: u16,
        client_port: u16,
        response: C::OneshotSender<Result<(), Error>>,
    },
    QueryRebootFlag(C::OneshotSender<Result<crate::protocol::sd::RebootFlag, Error>>),
    /// Test-only: force `sd_session_has_wrapped` to simulate the state a
    /// long-running client reaches after its SD session counter wraps past
    /// `0xFFFF`, without actually sending 65k SD messages. Fires the
    /// accompanying oneshot once the mutation is applied.
    #[cfg(all(test, feature = "client-tokio"))]
    ForceSdSessionWrappedForTest(bool, C::OneshotSender<Result<(), Error>>),
}

impl<P: PayloadWireFormat + 'static, C: ChannelFactory> core::fmt::Debug for ControlMessage<P, C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::SetInterface(addr, _) => f.debug_tuple("SetInterface").field(addr).finish(),
            Self::BindDiscovery(_) => f.write_str("BindDiscovery"),
            Self::UnbindDiscovery(_) => f.write_str("UnbindDiscovery"),
            Self::SendSD(addr, header, _) => {
                f.debug_tuple("SendSD").field(addr).field(header).finish()
            }
            Self::AddEndpoint(sid, iid, addr, local_port, _) => f
                .debug_tuple("AddEndpoint")
                .field(sid)
                .field(iid)
                .field(addr)
                .field(local_port)
                .finish(),
            Self::RemoveEndpoint(sid, iid, _) => f
                .debug_tuple("RemoveEndpoint")
                .field(sid)
                .field(iid)
                .finish(),
            Self::SendToService {
                service_id,
                instance_id,
                message,
                ..
            } => f
                .debug_struct("SendToService")
                .field("service_id", service_id)
                .field("instance_id", instance_id)
                .field("message", message)
                .finish_non_exhaustive(),
            Self::Subscribe {
                service_id,
                instance_id,
                event_group_id,
                ..
            } => f
                .debug_struct("Subscribe")
                .field("service_id", service_id)
                .field("instance_id", instance_id)
                .field("event_group_id", event_group_id)
                .finish_non_exhaustive(),
            Self::QueryRebootFlag(_) => f.write_str("QueryRebootFlag"),
            #[cfg(all(test, feature = "client-tokio"))]
            Self::ForceSdSessionWrappedForTest(b, _) => f
                .debug_tuple("ForceSdSessionWrappedForTest")
                .field(b)
                .finish(),
        }
    }
}

impl<P, C> ControlMessage<P, C>
where
    P: PayloadWireFormat + Send + 'static,
    C: ChannelFactory,
    Result<(), Error>: crate::transport::OneshotPooled<C>,
    Result<P, Error>: crate::transport::OneshotPooled<C>,
    Result<crate::protocol::sd::RebootFlag, Error>: crate::transport::OneshotPooled<C>,
{
    #[must_use]
    pub fn set_interface(interface: Ipv4Addr) -> (C::OneshotReceiver<Result<(), Error>>, Self) {
        let (sender, receiver) = C::oneshot();
        (receiver, Self::SetInterface(interface, sender))
    }
    #[must_use]
    pub fn bind_discovery() -> (C::OneshotReceiver<Result<(), Error>>, Self) {
        let (sender, receiver) = C::oneshot();
        (receiver, Self::BindDiscovery(sender))
    }
    #[must_use]
    pub fn unbind_discovery() -> (C::OneshotReceiver<Result<(), Error>>, Self) {
        let (sender, receiver) = C::oneshot();
        (receiver, Self::UnbindDiscovery(sender))
    }

    #[must_use]
    pub fn send_sd(
        socket_addr: SocketAddrV4,
        header: P::SdHeader,
    ) -> (C::OneshotReceiver<Result<(), Error>>, Self) {
        let (sender, receiver) = C::oneshot();
        (receiver, Self::SendSD(socket_addr, header, sender))
    }
    #[must_use]
    pub fn add_endpoint(
        service_id: u16,
        instance_id: u16,
        addr: SocketAddrV4,
        local_port: u16,
    ) -> (C::OneshotReceiver<Result<(), Error>>, Self) {
        let (sender, receiver) = C::oneshot();
        (
            receiver,
            Self::AddEndpoint(service_id, instance_id, addr, local_port, sender),
        )
    }

    #[must_use]
    pub fn remove_endpoint(
        service_id: u16,
        instance_id: u16,
    ) -> (C::OneshotReceiver<Result<(), Error>>, Self) {
        let (sender, receiver) = C::oneshot();
        (
            receiver,
            Self::RemoveEndpoint(service_id, instance_id, sender),
        )
    }

    #[allow(clippy::type_complexity)]
    #[must_use]
    pub fn send_to_service(
        service_id: u16,
        instance_id: u16,
        message: Message<P>,
    ) -> (
        C::OneshotReceiver<Result<(), Error>>,
        C::OneshotReceiver<Result<P, Error>>,
        Self,
    ) {
        let (send_complete_tx, send_complete_rx) = C::oneshot();
        let (response_tx, response_rx) = C::oneshot();
        (
            send_complete_rx,
            response_rx,
            Self::SendToService {
                service_id,
                instance_id,
                message,
                send_complete: send_complete_tx,
                response: response_tx,
            },
        )
    }

    #[must_use]
    pub fn subscribe(
        service_id: u16,
        instance_id: u16,
        major_version: u8,
        ttl: u32,
        event_group_id: u16,
        client_port: u16,
    ) -> (C::OneshotReceiver<Result<(), Error>>, Self) {
        let (sender, receiver) = C::oneshot();
        (
            receiver,
            Self::Subscribe {
                service_id,
                instance_id,
                major_version,
                ttl,
                event_group_id,
                client_port,
                response: sender,
            },
        )
    }

    #[must_use]
    pub fn query_reboot_flag() -> (
        C::OneshotReceiver<Result<crate::protocol::sd::RebootFlag, Error>>,
        Self,
    ) {
        let (sender, receiver) = C::oneshot();
        (receiver, Self::QueryRebootFlag(sender))
    }

    #[cfg(all(test, feature = "client-tokio"))]
    #[must_use]
    pub fn force_sd_session_wrapped_for_test(
        wrapped: bool,
    ) -> (C::OneshotReceiver<Result<(), Error>>, Self) {
        let (sender, receiver) = C::oneshot();
        (
            receiver,
            Self::ForceSdSessionWrappedForTest(wrapped, sender),
        )
    }

    /// Consume this message and notify its oneshot senders with
    /// `Error::Capacity(structure_name)` instead of silently dropping them.
    ///
    /// Dropping the senders would let the awaiting `oneshot::Receiver`s
    /// resolve to `RecvError`, which the public APIs currently `.unwrap()`
    /// — that would panic callers under load. Delivering an explicit
    /// `Err(Error::Capacity(..))` turns a would-be panic into a normal
    /// `Result` with a stable, descriptive error.
    fn reject_with_capacity(self, structure_name: &'static str) {
        match self {
            Self::SetInterface(_, response)
            | Self::BindDiscovery(response)
            | Self::UnbindDiscovery(response)
            | Self::SendSD(_, _, response)
            | Self::AddEndpoint(_, _, _, _, response)
            | Self::RemoveEndpoint(_, _, response)
            | Self::Subscribe { response, .. } => {
                let _ = response.send(Err(Error::Capacity(structure_name)));
            }
            Self::SendToService {
                send_complete,
                response,
                ..
            } => {
                let _ = send_complete.send(Err(Error::Capacity(structure_name)));
                let _ = response.send(Err(Error::Capacity(structure_name)));
            }
            Self::QueryRebootFlag(response) => {
                let _ = response.send(Err(Error::Capacity(structure_name)));
            }
            #[cfg(all(test, feature = "client-tokio"))]
            Self::ForceSdSessionWrappedForTest(_, response) => {
                let _ = response.send(Err(Error::Capacity(structure_name)));
            }
        }
    }
}

pub(super) struct Inner<
    PayloadDefinitions: PayloadWireFormat + 'static,
    Tm: Timer,
    R: E2ERegistryHandle,
    C: ChannelFactory,
    D,
> {
    /// MPSC Receiver used to receive control messages from outer client
    control_receiver: C::BoundedReceiver<ControlMessage<PayloadDefinitions, C>, 4>,
    /// Queue of pending control messages to process
    request_queue: Deque<ControlMessage<PayloadDefinitions, C>, REQUEST_QUEUE_CAP>,
    /// Pending request-responses keyed by `request_id` (`client_id` << 16 | `session_counter`).
    /// Set by `SendToService`, cleared when a matching unicast arrives.
    pending_responses: FnvIndexMap<
        u32,
        C::OneshotSender<Result<PayloadDefinitions, Error>>,
        PENDING_RESPONSES_CAP,
    >,
    /// Unbounded sender used to send updates to outer client
    update_sender: C::UnboundedSender<ClientUpdate<PayloadDefinitions>>,
    /// Target interface for sockets
    interface: Ipv4Addr,
    /// Socket manager for service discovery if bound
    discovery_socket: Option<SocketManager<PayloadDefinitions, C>>,
    /// Socket managers for unicast messages, keyed by local port
    unicast_sockets: FnvIndexMap<u16, SocketManager<PayloadDefinitions, C>, UNICAST_SOCKETS_CAP>,
    /// Per-sender SD session state for reboot detection
    session_tracker: SessionTracker,
    /// Registry of known service endpoints (auto-populated from SD + manual)
    service_registry: ServiceRegistry,
    /// Internal flag to continue run loop
    run: bool,
    /// Client ID for SOME/IP request headers (upper 16 bits of request ID)
    client_id: u16,
    /// Incrementing session counter for SOME/IP request headers (lower 16 bits of request ID)
    session_counter: u16,
    /// SD session state persisted across discovery socket rebinds so that
    /// `unbind_discovery` + `bind_discovery` does not emit a false reboot signal.
    sd_session_id: u16,
    sd_session_has_wrapped: bool,
    /// Shared E2E registry for runtime E2E configuration
    e2e_registry: R,
    /// Enable multicast loopback on SD sockets for same-host testing
    multicast_loopback: bool,
    /// Bind dispatch — abstracts the bind-and-spawn step over either a
    /// [`Spawner`](crate::transport::Spawner) (Send-required) or a
    /// [`LocalSpawner`](crate::transport::LocalSpawner) (single-task)
    /// path. Holds the [`TransportFactory`](crate::transport::TransportFactory)
    /// and the spawner internally; see
    /// [`crate::client::bind_dispatch`] for the two impls.
    dispatch: D,
    /// Async sleep primitive used by the run-loop's idle tick and any
    /// future periodic-emission paths. On `client-tokio` builds this is
    /// [`TokioTimer`] (which wraps `tokio::time::sleep`).
    timer: Tm,
    /// Phantom data to represent the generic message definitions
    phantom: core::marker::PhantomData<PayloadDefinitions>,
}

impl<P: PayloadWireFormat, Tm: Timer, R: E2ERegistryHandle, C: ChannelFactory, D> core::fmt::Debug
    for Inner<P, Tm, R, C, D>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Inner")
            .field("interface", &self.interface)
            .field("session_tracker", &self.session_tracker)
            .field("run", &self.run)
            .field("client_id", &self.client_id)
            .field("session_counter", &self.session_counter)
            .finish_non_exhaustive()
    }
}

impl<PayloadDefinitions, Tm, R, C, D> Inner<PayloadDefinitions, Tm, R, C, D>
where
    PayloadDefinitions: PayloadWireFormat + Clone + core::fmt::Debug + Send + 'static,
    Tm: Timer + 'static,
    R: E2ERegistryHandle,
    C: ChannelFactory,
    D: crate::client::bind_dispatch::BindDispatch<PayloadDefinitions, C, R> + 'static,
    // Channel-bound bundle (see comment in `client::mod`).
    Result<(), Error>: crate::transport::OneshotPooled<C>,
    Result<PayloadDefinitions, Error>: crate::transport::OneshotPooled<C>,
    Result<crate::protocol::sd::RebootFlag, Error>: crate::transport::OneshotPooled<C>,
    ControlMessage<PayloadDefinitions, C>: crate::transport::BoundedPooled<C, 4>,
    super::socket_manager::SendMessage<PayloadDefinitions, C>:
        crate::transport::BoundedPooled<C, { super::CLIENT_SOCKET_CHANNEL_CAP }>,
    Result<super::socket_manager::ReceivedMessage<PayloadDefinitions>, Error>:
        crate::transport::BoundedPooled<C, { super::CLIENT_SOCKET_CHANNEL_CAP }>,
    super::ClientUpdate<PayloadDefinitions>: crate::transport::UnboundedPooled<C>,
{
    /// Construct an `Inner` and return the control/update channels plus
    /// the run-loop future.
    ///
    /// The dispatch is one of [`SpawnerDispatch`] (Send-required) or
    /// [`LocalSpawnerDispatch`] (single-task) — the
    /// `Client::new_with_deps` / `Client::new_with_deps_local` public
    /// constructors pick the right one. The returned future inherits
    /// the dispatch's auto-trait set: `Send` if the dispatch is
    /// Send-aware and all dependencies are `Send`, `!Send` otherwise.
    ///
    /// [`SpawnerDispatch`]: super::bind_dispatch::SpawnerDispatch
    /// [`LocalSpawnerDispatch`]: super::bind_dispatch::LocalSpawnerDispatch
    #[allow(clippy::type_complexity)]
    pub fn build(
        interface: Ipv4Addr,
        e2e_registry: R,
        multicast_loopback: bool,
        dispatch: D,
        timer: Tm,
    ) -> (
        C::BoundedSender<ControlMessage<PayloadDefinitions, C>, 4>,
        C::UnboundedReceiver<ClientUpdate<PayloadDefinitions>>,
        impl core::future::Future<Output = ()> + 'static,
    ) {
        Self::build_with_pre_bound(interface, e2e_registry, multicast_loopback, dispatch, timer, None, None)
    }

    /// Like [`Self::build`] but with optional pre-bound sockets.
    ///
    /// When a caller has already bound the discovery and / or one
    /// unicast socket externally (typical of bare-metal embassy
    /// integrations using
    /// [`SocketManager::bind_discovery_seeded_with_transport_unspawned`]
    /// / [`SocketManager::bind_with_transport_unspawned`]), the pre-bound
    /// [`SocketManager`] handles can be threaded in here so the run-loop
    /// never has to call `dispatch.bind_*` — letting callers wire the
    /// crate without a [`crate::Spawner`] impl.
    ///
    /// `pre_bound_discovery` is set into `discovery_socket`;
    /// `pre_bound_unicast` is inserted into `unicast_sockets` keyed by
    /// its `local_port()`. After construction the run-loop's
    /// `bind_discovery` / `bind_unicast` no-op when their target is
    /// already set, so subsequent `subscribe_no_wait` calls flow without
    /// invoking the dispatch.
    #[allow(clippy::type_complexity)]
    pub fn build_with_pre_bound(
        interface: Ipv4Addr,
        e2e_registry: R,
        multicast_loopback: bool,
        dispatch: D,
        timer: Tm,
        pre_bound_discovery: Option<super::socket_manager::SocketManager<PayloadDefinitions, C>>,
        pre_bound_unicast: Option<super::socket_manager::SocketManager<PayloadDefinitions, C>>,
    ) -> (
        C::BoundedSender<ControlMessage<PayloadDefinitions, C>, 4>,
        C::UnboundedReceiver<ClientUpdate<PayloadDefinitions>>,
        impl core::future::Future<Output = ()> + 'static,
    ) {
        info!("Initializing SOME/IP Client");
        let (control_sender, control_receiver) = C::bounded::<_, 4>();
        let (update_sender, update_receiver) = C::unbounded();
        let mut unicast_sockets: FnvIndexMap<u16, super::socket_manager::SocketManager<PayloadDefinitions, C>, { UNICAST_SOCKETS_CAP }> = FnvIndexMap::new();
        if let Some(mgr) = pre_bound_unicast {
            let port = mgr.port();
            let _ = unicast_sockets.insert(port, mgr);
        }
        let inner = Self {
            control_receiver,
            request_queue: Deque::new(),
            pending_responses: FnvIndexMap::new(),
            update_sender,
            interface,
            discovery_socket: pre_bound_discovery,
            unicast_sockets,
            session_tracker: SessionTracker::default(),
            service_registry: ServiceRegistry::default(),
            run: true,
            client_id: 0x1234,
            session_counter: 1,
            sd_session_id: 1,
            sd_session_has_wrapped: false,
            e2e_registry,
            multicast_loopback,
            dispatch,
            timer,
            phantom: core::marker::PhantomData,
        };
        (control_sender, update_receiver, inner.run_future())
    }

    async fn bind_discovery(&mut self) -> Result<(), Error> {
        if self.discovery_socket.is_some() {
            Ok(())
        } else {
            let socket = self
                .dispatch
                .bind_discovery(
                    self.interface,
                    self.e2e_registry.clone(),
                    self.sd_session_id,
                    self.sd_session_has_wrapped,
                    self.multicast_loopback,
                )
                .await?;
            self.discovery_socket = Some(socket);
            Ok(())
        }
    }

    // Dropping the receiver kills the loop
    async fn unbind_discovery(&mut self) {
        debug!("Unbinding Discovery socket.");
        if let Some(socket) = self.discovery_socket.take() {
            self.sd_session_id = socket.session_id();
            self.sd_session_has_wrapped =
                socket.reboot_flag() == crate::protocol::sd::RebootFlag::Continuous;
            socket.shut_down().await;
        }
    }

    fn set_interface(&mut self, interface: Ipv4Addr) {
        self.interface = interface;
    }

    async fn bind_unicast(&mut self, port: u16) -> Result<u16, Error> {
        if port != 0
            && let Some(socket) = self.unicast_sockets.get(&port)
        {
            return Ok(socket.port());
        }
        // Check capacity before asking the OS for a port so we don't
        // bind-then-drop a socket we can't track.
        if self.unicast_sockets.len() >= UNICAST_SOCKETS_CAP {
            warn!(
                "unicast_sockets at capacity ({}); refusing new bind of port {}",
                UNICAST_SOCKETS_CAP, port
            );
            return Err(Error::Capacity("unicast_sockets"));
        }
        let unicast_socket = self
            .dispatch
            .bind_unicast(port, self.e2e_registry.clone())
            .await?;
        let bound_port = unicast_socket.port();
        // Capacity was checked above, so insert cannot report "full" here.
        // A defensive check guards against a future refactor that changes
        // the ordering.
        if self
            .unicast_sockets
            .insert(bound_port, unicast_socket)
            .is_err()
        {
            error!(
                "unicast_sockets insert failed after capacity check passed — invariant violation"
            );
            return Err(Error::Capacity("unicast_sockets"));
        }
        debug!("Bound unicast socket on port {}", bound_port);
        Ok(bound_port)
    }

    /// Tracks the caller's response channel against `request_id` so a
    /// future unicast reply can be routed back. If the
    /// `pending_responses` map is already at `PENDING_RESPONSES_CAP`, the
    /// `response` sender is recovered from the failed `insert` and used
    /// to deliver `Err(Error::Capacity("pending_responses"))` — the
    /// caller's `PendingResponse::response().await` resolves cleanly
    /// instead of panicking on the `RecvError` that dropping the Sender
    /// would have produced. If `request_id` is reused while an older
    /// pending entry still exists (e.g. after a `session_counter`
    /// wrap-around), the displaced sender is likewise completed with
    /// `Err(Error::Capacity("pending_responses"))` rather than being
    /// silently dropped — the caller awaiting the previous request
    /// sees a clean error instead of a `RecvError` panic. Any reply
    /// that later arrives for a dropped `request_id` is surfaced on
    /// the update stream via `ClientUpdate::Unicast` instead of
    /// matching a pending entry.
    fn track_or_reject_pending_response(
        &mut self,
        request_id: u32,
        response: C::OneshotSender<Result<PayloadDefinitions, Error>>,
    ) {
        match self.pending_responses.insert(request_id, response) {
            Ok(None) => {}
            Ok(Some(displaced_response)) => {
                // `request_id` reuse is expected once `session_counter`
                // wraps every ~65k requests on a long-lived client, and
                // legitimate when the previous request is still pending.
                // The displaced sender carries `Error::Capacity` to its
                // awaiter; logging at `warn!` per wrap floods ops dashboards
                // for a routine event, so demote to `debug!`.
                debug!(
                    "pending_responses already contained request_id \
                     0x{:08X}; replacing existing pending response",
                    request_id
                );
                let _ = displaced_response.send(Err(Error::Capacity("pending_responses")));
            }
            Err((_req_id, response)) => {
                warn!(
                    "pending_responses at capacity ({}); response tracking \
                     dropped for request_id 0x{:08X}",
                    PENDING_RESPONSES_CAP, request_id
                );
                let _ = response.send(Err(Error::Capacity("pending_responses")));
            }
        }
    }

    async fn receive_discovery(
        socket_manager: &mut Option<SocketManager<PayloadDefinitions, C>>,
    ) -> Result<
        (
            SocketAddr,
            protocol::Header,
            <PayloadDefinitions as PayloadWireFormat>::SdHeader,
        ),
        Error,
    > {
        let Some(socket) = socket_manager else {
            // If we don't have a receiver, return a future that never resolves
            return future::pending().await;
        };
        let Some(result) = socket.receive().await else {
            // Socket loop has exited. Evict the dead manager so
            // subsequent polls don't busy-loop on a closed receiver —
            // instead they fall through to the `future::pending()`
            // arm and wait until the user re-binds discovery (e.g.
            // via SetInterface).
            *socket_manager = None;
            return Err(Error::SocketClosedUnexpectedly);
        };
        let received = result?;
        let someip_header = received.message.header().clone();
        if let Some(sd_header) = received.message.sd_header() {
            Ok((received.source, someip_header, Clone::clone(sd_header)))
        } else {
            Err(Error::UnexpectedDiscoveryMessage(someip_header))
        }
    }

    /// Receive from any bound unicast socket. Returns the first message ready
    /// from any socket. If no sockets are bound, returns a future that never resolves.
    ///
    /// A unicast socket whose loop has exited (`poll_receive` returns
    /// `Poll::Ready(None)`) is evicted from the map immediately rather
    /// than having `Err(SocketClosedUnexpectedly)` returned once per
    /// poll forever, which would CPU-pin the run-loop and flood the
    /// update stream.
    async fn receive_any_unicast(
        unicast_sockets: &mut FnvIndexMap<
            u16,
            SocketManager<PayloadDefinitions, C>,
            UNICAST_SOCKETS_CAP,
        >,
    ) -> Result<ReceivedMessage<PayloadDefinitions>, Error> {
        if unicast_sockets.is_empty() {
            return future::pending().await;
        }

        core::future::poll_fn(|cx| {
            // Collect ports of any sockets that report `Ready(None)`
            // (loop has exited). Evict them after the iteration so we
            // do not mutate the map while iterating it.
            let mut dead_ports: heapless::Vec<u16, UNICAST_SOCKETS_CAP> = heapless::Vec::new();
            let mut delivered: Option<Result<ReceivedMessage<PayloadDefinitions>, Error>> = None;
            for (port, socket) in unicast_sockets.iter_mut() {
                if let Poll::Ready(result) = socket.poll_receive(cx) {
                    match result {
                        Some(msg) => {
                            delivered = Some(msg);
                            break;
                        }
                        None => {
                            // Mark for eviction; keep scanning others.
                            let _ = dead_ports.push(*port);
                        }
                    }
                }
            }
            for port in &dead_ports {
                unicast_sockets.remove(port);
                crate::log::warn!("Unicast socket on port {port} closed; evicted from registry");
            }
            if let Some(msg) = delivered {
                Poll::Ready(msg)
            } else if unicast_sockets.is_empty() {
                // The last socket just got evicted; fall through to a
                // pending state so the next bind triggers a fresh poll.
                Poll::Pending
            } else if !dead_ports.is_empty() {
                // At least one socket got evicted but others remain;
                // re-poll so the caller observes the next ready event
                // promptly instead of waiting on a stale waker.
                cx.waker().wake_by_ref();
                Poll::Pending
            } else {
                Poll::Pending
            }
        })
        .await
    }

    #[allow(clippy::too_many_lines)]
    async fn handle_control_message(&mut self) {
        if let Some(active_request) = self.request_queue.pop_front() {
            match active_request {
                ControlMessage::SetInterface(interface, response) => {
                    if self.discovery_socket.is_some() {
                        info!(
                            "Discovery socket currently bound to interface: {}, unbinding.",
                            self.interface
                        );
                        self.unbind_discovery().await;
                        // Re-enqueue after pop. The slot we popped is free,
                        // so `push_front` should never fail here — but if a
                        // future refactor breaks that invariant, reject via
                        // the capacity path instead of silently dropping the
                        // response oneshot (matches the primary `push_back`
                        // overflow arm in the control-channel receiver).
                        if let Err(rejected) = self
                            .request_queue
                            .push_front(ControlMessage::SetInterface(interface, response))
                        {
                            error!("request_queue push_front failed after pop — invariant broken");
                            rejected.reject_with_capacity("request_queue");
                        }
                        return;
                    }
                    if self.interface != interface {
                        self.set_interface(interface);
                        // See re-enqueue note above.
                        if let Err(rejected) = self
                            .request_queue
                            .push_front(ControlMessage::SetInterface(interface, response))
                        {
                            error!("request_queue push_front failed after pop — invariant broken");
                            rejected.reject_with_capacity("request_queue");
                        }
                        return;
                    }
                    // Reaching here: discovery is not bound AND
                    // `interface == self.interface`. Do nothing — the
                    // user expressed no change of intent. Previously
                    // this branch silently called `bind_discovery()`
                    // as a side effect, which surprised callers
                    // probing the current interface via
                    // `client.set_interface(client.interface()).await`.
                    debug!("SetInterface: no-op (interface unchanged, discovery not bound)");
                    if response.send(Ok(())).is_err() {
                        debug!("SetInterface: caller dropped the response receiver");
                    }
                }
                ControlMessage::BindDiscovery(response) => {
                    let result = self.bind_discovery().await;
                    if response.send(result).is_err() {
                        debug!("BindDiscovery: caller dropped the response receiver");
                    }
                }
                ControlMessage::UnbindDiscovery(response) => {
                    self.unbind_discovery().await;
                    if response.send(Ok(())).is_err() {
                        debug!("UnbindDiscovery: caller dropped the response receiver");
                    }
                }
                ControlMessage::SendSD(target, header, response) => {
                    // SD Message, If the discovery socket is not bound, bind it
                    match &mut self.discovery_socket {
                        None => {
                            match self.bind_discovery().await {
                                Ok(()) => {
                                    // See re-enqueue note on SetInterface above.
                                    if let Err(rejected) = self.request_queue.push_front(
                                        ControlMessage::SendSD(target, header, response),
                                    ) {
                                        error!(
                                            "request_queue push_front failed after pop — invariant broken"
                                        );
                                        rejected.reject_with_capacity("request_queue");
                                    }
                                }
                                Err(e) => {
                                    error!(
                                        "Failed to bind discovery socket for sending SD message: {:?}",
                                        e
                                    );
                                    if response.send(Err(e)).is_err() {
                                        debug!(
                                            "SendSD (bind-err path): caller dropped the response receiver"
                                        );
                                    }
                                }
                            }
                        }
                        Some(discovery_socket) => {
                            let message = Message::<PayloadDefinitions>::new_sd(
                                u32::from(discovery_socket.session_id()),
                                &header,
                            );
                            debug!("Sending {:?} to {}", &message, target);
                            let send_result = self
                                .discovery_socket
                                .as_mut()
                                .unwrap()
                                .send(target, message)
                                .await;
                            if response.send(send_result).is_err() {
                                debug!("SendSD: caller dropped the response receiver");
                            }
                        }
                    }
                }
                ControlMessage::AddEndpoint(
                    service_id,
                    instance_id,
                    addr,
                    local_port,
                    response,
                ) => {
                    let insert_result = self.service_registry.insert(
                        ServiceInstanceId {
                            service_id,
                            instance_id,
                        },
                        ServiceEndpointInfo {
                            addr,
                            local_port,
                            major_version: 0xFF,
                            minor_version: 0xFFFF_FFFF,
                        },
                    );
                    let outcome = if insert_result.is_ok() {
                        debug!(
                            "Added endpoint for service 0x{:04X}.0x{:04X} -> {}",
                            service_id, instance_id, addr,
                        );
                        Ok(())
                    } else {
                        warn!(
                            "service_registry at capacity ({}); cannot add 0x{:04X}.0x{:04X}",
                            crate::client::service_registry::SERVICE_REGISTRY_CAP,
                            service_id,
                            instance_id,
                        );
                        Err(Error::Capacity("service_registry"))
                    };
                    if response.send(outcome).is_err() {
                        debug!("AddEndpoint: caller dropped the response receiver");
                    }
                }
                ControlMessage::RemoveEndpoint(service_id, instance_id, response) => {
                    self.service_registry.remove(ServiceInstanceId {
                        service_id,
                        instance_id,
                    });
                    debug!(
                        "Removed endpoint for service 0x{:04X}.0x{:04X}",
                        service_id, instance_id,
                    );
                    if response.send(Ok(())).is_err() {
                        debug!("RemoveEndpoint: caller dropped the response receiver");
                    }
                }
                ControlMessage::SendToService {
                    service_id,
                    instance_id,
                    mut message,
                    send_complete,
                    response,
                } => {
                    let id = ServiceInstanceId {
                        service_id,
                        instance_id,
                    };
                    let Some(endpoint) = self.service_registry.get(id) else {
                        let _ = send_complete.send(Err(Error::ServiceNotFound));
                        return;
                    };
                    let target = endpoint.addr;
                    let desired_port = endpoint.local_port;

                    let source_port = if desired_port == 0 {
                        // Ephemeral: auto-bind only if no sockets exist, then use first
                        if self.unicast_sockets.is_empty() {
                            match self.bind_unicast(0).await {
                                Ok(port) => {
                                    debug!("Auto-bound unicast on port {} for SendToService", port);
                                    port
                                }
                                Err(e) => {
                                    let _ = send_complete.send(Err(e));
                                    return;
                                }
                            }
                        } else {
                            *self.unicast_sockets.keys().next().unwrap()
                        }
                    } else {
                        // Specific port: bind if not already bound
                        match self.bind_unicast(desired_port).await {
                            Ok(port) => port,
                            Err(e) => {
                                let _ = send_complete.send(Err(e));
                                return;
                            }
                        }
                    };
                    let socket = self.unicast_sockets.get_mut(&source_port).unwrap();

                    // Stamp request ID with the CURRENT session counter,
                    // but only advance it on successful send. A failed
                    // send should not chew through the 16-bit session
                    // space — under transient transport failure that
                    // could wrap toward in-flight pending_responses
                    // far faster than expected.
                    let request_id =
                        (u32::from(self.client_id) << 16) | u32::from(self.session_counter);
                    message.set_request_id(request_id);

                    let send_result = socket.send(target, message).await;
                    match send_result {
                        Ok(()) => {
                            // Advance the counter only after a real
                            // wire transmission. Skip 0 on wrap.
                            self.session_counter = self.session_counter.wrapping_add(1);
                            if self.session_counter == 0 {
                                self.session_counter = 1;
                            }
                            let _ = send_complete.send(Ok(()));
                            self.track_or_reject_pending_response(request_id, response);
                        }
                        Err(e) => {
                            let _ = send_complete.send(Err(e));
                        }
                    }
                }
                #[cfg(all(test, feature = "client-tokio"))]
                ControlMessage::ForceSdSessionWrappedForTest(wrapped, response) => {
                    self.sd_session_has_wrapped = wrapped;
                    let _ = response.send(Ok(()));
                }
                ControlMessage::QueryRebootFlag(response) => {
                    // Prefer the live socket's tracked flag when bound. When
                    // unbound, fall back to `sd_session_has_wrapped`, which
                    // persists wrap state across unbind/rebind (updated in
                    // `unbind_discovery` from the socket manager before it's
                    // dropped). Without this fallback, a long-running client
                    // that wraps past 0xFFFF and then unbinds discovery
                    // would erroneously revert to `RecentlyRebooted` on the
                    // next `reboot_flag()` call.
                    let flag = if let Some(socket) = self.discovery_socket.as_ref() {
                        socket.reboot_flag()
                    } else if self.sd_session_has_wrapped {
                        crate::protocol::sd::RebootFlag::Continuous
                    } else {
                        crate::protocol::sd::RebootFlag::RecentlyRebooted
                    };
                    if response.send(Ok(flag)).is_err() {
                        debug!("QueryRebootFlag: caller dropped the response receiver");
                    }
                }
                ControlMessage::Subscribe {
                    service_id,
                    instance_id,
                    major_version,
                    ttl,
                    event_group_id,
                    client_port,
                    response,
                } => {
                    // Look up endpoint from service registry
                    let id = ServiceInstanceId {
                        service_id,
                        instance_id,
                    };
                    if self.service_registry.get(id).is_none() {
                        if response.send(Err(Error::ServiceNotFound)).is_err() {
                            debug!(
                                "Subscribe (ServiceNotFound): caller dropped the response receiver (expected for subscribe_no_wait)"
                            );
                        }
                        return;
                    }

                    // Bind unicast on the requested port (0 = ephemeral)
                    let unicast_port = match self.bind_unicast(client_port).await {
                        Ok(port) => {
                            debug!("Bound unicast on port {} for Subscribe", port);
                            port
                        }
                        Err(e) => {
                            if response.send(Err(e)).is_err() {
                                debug!(
                                    "Subscribe (bind-err): caller dropped the response receiver"
                                );
                            }
                            return;
                        }
                    };

                    // Auto-bind discovery if not bound (re-queue like SendSD does)
                    match &mut self.discovery_socket {
                        None => match self.bind_discovery().await {
                            Ok(()) => {
                                // Re-enqueue the Subscribe carrying the
                                // ALREADY-bound `unicast_port` so pass-2
                                // hits the `bind_unicast` dedupe path
                                // instead of allocating a second
                                // ephemeral socket. Carrying the
                                // original `client_port=0` would
                                // re-bind ephemerally and leak the
                                // original socket into
                                // `unicast_sockets` until the slot cap
                                // hit.
                                if let Err(rejected) =
                                    self.request_queue.push_front(ControlMessage::Subscribe {
                                        service_id,
                                        instance_id,
                                        major_version,
                                        ttl,
                                        event_group_id,
                                        client_port: unicast_port,
                                        response,
                                    })
                                {
                                    error!(
                                        "request_queue push_front failed after pop — invariant broken"
                                    );
                                    rejected.reject_with_capacity("request_queue");
                                }
                            }
                            Err(e) => {
                                if response.send(Err(e)).is_err() {
                                    debug!(
                                        "Subscribe (discovery-bind-err): caller dropped the response receiver"
                                    );
                                }
                            }
                        },
                        Some(discovery_socket) => {
                            let sd_header = PayloadDefinitions::new_subscription_sd_header(
                                service_id,
                                instance_id,
                                major_version,
                                ttl,
                                event_group_id,
                                self.interface,
                                crate::protocol::sd::TransportProtocol::Udp,
                                unicast_port,
                                discovery_socket.reboot_flag(),
                            );
                            let session_id = u32::from(discovery_socket.session_id());
                            let message =
                                Message::<PayloadDefinitions>::new_sd(session_id, &sd_header);
                            let reg = self.service_registry.get(id).unwrap();
                            let target =
                                SocketAddrV4::new(*reg.addr.ip(), protocol::sd::MULTICAST_PORT);
                            debug!("Sending Subscribe {:?} to {}", &message, target);
                            let send_result = self
                                .discovery_socket
                                .as_mut()
                                .unwrap()
                                .send(target, message)
                                .await;
                            if response.send(send_result).is_err() {
                                debug!(
                                    "Subscribe: caller dropped the response receiver (expected for subscribe_no_wait)"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn run_future(mut self) {
        info!("SOME/IP Client processing loop started");
        loop {
            // Scope the `&mut self` destructure + pinned per-iteration
            // futures so all borrows of `self` drop before we call
            // `self.handle_control_message().await` below. `pin_mut!`
            // creates stack-pinned locals that outlive the select
            // macro, so the inner block is required to release those
            // borrows.
            let should_break = {
                let Self {
                    control_receiver,
                    pending_responses,
                    discovery_socket,
                    unicast_sockets,
                    update_sender,
                    request_queue,
                    session_tracker,
                    service_registry,
                    run,
                    timer,
                    ..
                } = &mut self;
                // Build fresh per-iteration futures and fuse them for
                // `select!`'s `FusedFuture + Unpin` bound.
                // `receive_discovery` / `receive_any_unicast` are
                // async fns that are not `Unpin`; the `Timer::sleep`
                // future likewise. Stack-pinning via `pin_mut!`
                // satisfies both.
                //
                // The 125ms idle tick goes through the caller-supplied
                // `Timer` impl. On `client-tokio` builds this is
                // `TokioTimer` (wrapping `tokio::time::sleep`); bare-metal
                // builds plug in their own (e.g. an `embassy_time` shim).
                let control_fut = control_receiver.recv().fuse();
                let sleep_fut = timer.sleep(core::time::Duration::from_millis(125)).fuse();
                let discovery_fut = Self::receive_discovery(discovery_socket).fuse();
                let unicast_fut = Self::receive_any_unicast(unicast_sockets).fuse();
                pin_mut!(control_fut, sleep_fut, discovery_fut, unicast_fut);

                // `select_biased!` (rather than `select!`) because
                // futures-util's pseudo-random `select!` requires
                // `std`. Top-down arm priority is intentional here:
                // `control_fut` sits first because control messages
                // drive loop lifecycle (shutdown, queue submissions)
                // and dropping them on the floor would deadlock the
                // caller's request path. Beyond control, the order
                // is `sleep_fut → discovery_fut → unicast_fut`; the
                // sleep arm is a 125 ms tick so it can't drive
                // sustained pressure, and discovery (multicast SD)
                // is bursty enough that unicast is not at real risk
                // of starvation in practice. If a future workload
                // proves otherwise, the per-iteration arm-flip
                // pattern used in `socket_manager`'s send/recv
                // select can be lifted here too.
                select_biased! {
                // Receive a control message
                ctrl = control_fut => {
                    if let Some(ctrl) = ctrl {
                        debug!("Received control message: {:?}", ctrl);
                        if let Err(rejected) = request_queue.push_back(ctrl) {
                            // Queue full: notify the rejected message's
                            // oneshot senders with `Error::Capacity` so
                            // callers see a typed overload error rather
                            // than a `RecvError` (which `client::mod`
                            // maps to `Error::Shutdown`, conflating
                            // overload with lifecycle failure).
                            warn!(
                                "request_queue at capacity ({}); rejecting control message with Capacity error",
                                REQUEST_QUEUE_CAP
                            );
                            rejected.reject_with_capacity("request_queue");
                        }
                    } else {
                        // The sender has been dropped, so we should exit
                        *run = false;
                    }
                }
                () = sleep_fut => {}
                // Receive a discovery message
                discovery = discovery_fut => {
                    trace!("Received discovery message: {:?}", discovery);
                    match discovery {
                        Ok((source, someip_header, sd_header)) => {
                            // Extract session ID from SOME/IP request_id (lower 16 bits)
                            let session_id = (someip_header.request_id() & 0xFFFF) as u16;
                            let sd_payload = PayloadDefinitions::new_sd_payload(&sd_header);
                            // Extract reboot flag from the SD payload flags
                            let reboot_flag = sd_payload
                                .sd_flags()
                                .map_or(crate::protocol::sd::RebootFlag::Continuous, |f| {
                                    f.reboot()
                                });

                            // Track sender session/reboot state for every SD entry
                            // that identifies a service instance, not only
                            // offer/stop-offer entries. This ensures reboot
                            // detection works for all SD traffic (FindService,
                            // Subscribe, SubscribeAck, etc.).
                            let mut rebooted = false;
                            sd_payload.for_each_service_instance(|svc_id, inst_id| {
                                let verdict = session_tracker.check(
                                    source,
                                    TransportKind::Multicast,
                                    svc_id,
                                    inst_id,
                                    session_id,
                                    reboot_flag,
                                );
                                if verdict == SessionVerdict::Reboot {
                                    rebooted = true;
                                }
                            });

                            // Auto-populate service registry from offer/stop-offer
                            // SD entries.
                            sd_payload.for_each_offered_endpoint(|ep| {
                                let id = ServiceInstanceId {
                                    service_id: ep.service_id,
                                    instance_id: ep.instance_id,
                                };
                                if ep.is_offer {
                                    if let Some(addr) = ep.addr {
                                        if service_registry
                                            .insert(
                                                id,
                                                ServiceEndpointInfo {
                                                    addr,
                                                    local_port: 0,
                                                    major_version: ep.major_version,
                                                    minor_version: ep.minor_version,
                                                },
                                            )
                                            .is_ok()
                                        {
                                            trace!(
                                                "Registry: added 0x{:04X}.0x{:04X} -> {}",
                                                ep.service_id, ep.instance_id, addr,
                                            );
                                        } else {
                                            warn!(
                                                "Registry full; dropped offer for 0x{:04X}.0x{:04X}",
                                                ep.service_id, ep.instance_id,
                                            );
                                        }
                                    }
                                } else {
                                    service_registry.remove(id);
                                    trace!(
                                        "Registry: removed 0x{:04X}.0x{:04X}",
                                        ep.service_id, ep.instance_id,
                                    );
                                }
                            });

                            if rebooted {
                                let _ = update_sender.send_now(ClientUpdate::SenderRebooted(source));
                            }

                            let discovery_msg = DiscoveryMessage {
                                source,
                                someip_header,
                                sd_header,
                            };
                            let _ = update_sender.send_now(ClientUpdate::DiscoveryUpdated(discovery_msg));
                        }
                        Err(err) => {
                            error!("Error receiving discovery message: {:?}", err);
                            let _ = update_sender.send_now(ClientUpdate::Error(err));
                        }
                    }
                 }
                 unicast = unicast_fut => {
                     trace!("Received unicast message: {:?}", unicast);
                     match unicast {
                         Ok(received) => {
                             let ReceivedMessage { message: received_message, e2e_status, .. } = received;
                             // Check if this matches a pending request-response by request_id
                             let request_id = received_message.header().request_id();
                             if let Some(sender) = pending_responses.remove(&request_id) {
                                 let _ = sender.send(Ok(received_message.payload().clone()));
                                 continue;
                             }
                             // Not a response — forward as ClientUpdate::Unicast
                             let _ = update_sender.send_now(ClientUpdate::Unicast { message: received_message, e2e_status });
                         }
                         Err(err) => {
                             let _ = update_sender.send_now(ClientUpdate::Error(err));
                         }
                     }
                 }
                }
                !*run
            };
            if should_break {
                info!("SOME/IP Client processing loop exiting");
                break;
            }
            self.handle_control_message().await;
        }
    }
}

#[cfg(all(test, feature = "client-tokio"))]
mod tests {
    use super::*;
    use crate::protocol::sd::test_support::{TestPayload, empty_sd_header};
    use crate::transport::{OneshotRecv, UnboundedRecv};
    use std::format;
    use tokio::sync::mpsc::Sender;
    use tokio::sync::{mpsc, oneshot};

    type TestControl = ControlMessage<TestPayload, TokioChannels>;
    /// Type alias for the fully-spelled `Inner` flavor used throughout
    /// these tests: tokio everything, default `Arc<Mutex<E2ERegistry>>`
    /// and `Arc<RwLock<Ipv4Addr>>` handles.
    type TestInner = Inner<
        TestPayload,
        crate::tokio_transport::TokioTimer,
        Arc<Mutex<E2ERegistry>>,
        TokioChannels,
        crate::client::bind_dispatch::SpawnerDispatch<
            crate::tokio_transport::TokioTransport,
            TokioSpawner,
        >,
    >;

    #[test]
    fn test_control_message_constructors() {
        // Each constructor returns (oneshot::Receiver, ControlMessage)
        let (_rx, msg) = TestControl::set_interface(Ipv4Addr::LOCALHOST);
        assert!(matches!(msg, ControlMessage::SetInterface(..)));

        let (_rx, msg) = TestControl::bind_discovery();
        assert!(matches!(msg, ControlMessage::BindDiscovery(..)));

        let (_rx, msg) = TestControl::unbind_discovery();
        assert!(matches!(msg, ControlMessage::UnbindDiscovery(..)));

        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1234);
        let sd_header = empty_sd_header();
        let (_rx, msg) = TestControl::send_sd(target, sd_header);
        assert!(matches!(msg, ControlMessage::SendSD(..)));

        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5000);
        let (_rx, msg) = TestControl::add_endpoint(0x1234, 0x0001, addr, 0);
        assert!(matches!(msg, ControlMessage::AddEndpoint(..)));

        let (_rx, msg) = TestControl::remove_endpoint(0x1234, 0x0001);
        assert!(matches!(msg, ControlMessage::RemoveEndpoint(..)));

        let message = Message::<TestPayload>::new_sd(1, &empty_sd_header());
        let (_send_rx, _resp_rx, msg) = TestControl::send_to_service(0x1234, 0x0001, message);
        assert!(matches!(msg, ControlMessage::SendToService { .. }));

        let (_rx, msg) = TestControl::subscribe(0x1234, 0x0001, 1, 3, 0x01, 0);
        assert!(matches!(msg, ControlMessage::Subscribe { .. }));
    }

    /// `reject_with_capacity` must notify every oneshot sender inside a
    /// rejected `ControlMessage` with `Err(Error::Capacity(..))` — for
    /// `SendToService`, _both_ the `send_complete` and `response`
    /// channels. Dropping either channel would let a caller's `.unwrap()`
    /// (or `.expect(...)` inside `PendingResponse::response()`) panic on
    /// the resulting `RecvError`, which is exactly what Copilot flagged.
    #[test]
    fn reject_with_capacity_notifies_every_sender() {
        use crate::transport::OneshotCancelled;
        use futures_util::FutureExt;

        fn expect_capacity<F>(rx: F, label: &str)
        where
            F: core::future::Future<Output = Result<Result<(), Error>, OneshotCancelled>>,
        {
            match rx.now_or_never() {
                Some(Ok(Err(Error::Capacity(s)))) => assert_eq!(s, "request_queue", "{label}"),
                other => panic!("{label}: expected Some(Ok(Err(Capacity))), got {other:?}"),
            }
        }

        // Variants carrying a single Result<(), Error> response sender.
        let (rx, msg) = TestControl::set_interface(Ipv4Addr::LOCALHOST);
        msg.reject_with_capacity("request_queue");
        expect_capacity(rx.recv(), "SetInterface");

        let (rx, msg) = TestControl::bind_discovery();
        msg.reject_with_capacity("request_queue");
        expect_capacity(rx.recv(), "BindDiscovery");

        let (rx, msg) = TestControl::unbind_discovery();
        msg.reject_with_capacity("request_queue");
        expect_capacity(rx.recv(), "UnbindDiscovery");

        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1234);
        let (rx, msg) = TestControl::send_sd(target, empty_sd_header());
        msg.reject_with_capacity("request_queue");
        expect_capacity(rx.recv(), "SendSD");

        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5000);
        let (rx, msg) = TestControl::add_endpoint(0x1234, 0x0001, addr, 0);
        msg.reject_with_capacity("request_queue");
        expect_capacity(rx.recv(), "AddEndpoint");

        let (rx, msg) = TestControl::remove_endpoint(0x1234, 0x0001);
        msg.reject_with_capacity("request_queue");
        expect_capacity(rx.recv(), "RemoveEndpoint");

        let (rx, msg) = TestControl::subscribe(0x1234, 0x0001, 1, 3, 0x01, 0);
        msg.reject_with_capacity("request_queue");
        expect_capacity(rx.recv(), "Subscribe");

        // SendToService carries two senders — both must be notified so that
        // neither `send_rx.recv().await.unwrap()?` nor `PendingResponse::response()`
        // panics.
        let message = Message::<TestPayload>::new_sd(1, &empty_sd_header());
        let (send_rx, resp_rx, msg) = TestControl::send_to_service(0x1234, 0x0001, message);
        msg.reject_with_capacity("request_queue");
        expect_capacity(send_rx.recv(), "SendToService.send_complete");
        // resp_rx has type Result<TestPayload, Error> — check it separately
        match resp_rx.recv().now_or_never() {
            Some(Ok(Err(Error::Capacity(s)))) => {
                assert_eq!(s, "request_queue", "SendToService.response");
            }
            other => {
                panic!("SendToService.response: expected Some(Ok(Err(Capacity))), got {other:?}")
            }
        }
    }

    #[test]
    fn test_control_message_debug() {
        let (_rx, msg) = TestControl::set_interface(Ipv4Addr::LOCALHOST);
        let s = format!("{msg:?}");
        assert!(s.contains("SetInterface"));

        let (_rx, msg) = TestControl::bind_discovery();
        assert!(!format!("{msg:?}").is_empty());

        let (_rx, msg) = TestControl::unbind_discovery();
        assert!(format!("{msg:?}").contains("UnbindDiscovery"));

        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1234);
        let sd_header = empty_sd_header();
        let (_rx, msg) = TestControl::send_sd(target, sd_header);
        assert!(format!("{msg:?}").contains("SendSD"));

        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5000);
        let (_rx, msg) = TestControl::add_endpoint(0x1234, 0x0001, addr, 0);
        let s = format!("{msg:?}");
        assert!(s.contains("AddEndpoint"));

        let (_rx, msg) = TestControl::remove_endpoint(0x1234, 0x0001);
        let s = format!("{msg:?}");
        assert!(s.contains("RemoveEndpoint"));

        let message = Message::<TestPayload>::new_sd(1, &empty_sd_header());
        let (_send_rx, _resp_rx, msg) = TestControl::send_to_service(0x1234, 0x0001, message);
        let s = format!("{msg:?}");
        assert!(s.contains("SendToService"));
        assert!(s.contains("service_id"));
        assert!(s.contains("instance_id"));

        let (_rx, msg) = TestControl::subscribe(0x1234, 0x0001, 1, 3, 0x01, 0);
        let s = format!("{msg:?}");
        assert!(s.contains("Subscribe"));
        assert!(s.contains("service_id"));
        assert!(s.contains("event_group_id"));
    }

    /// Build an [`Inner`] without spawning the run loop, for direct
    /// unit-testing of state-mutating methods.
    fn make_inner_for_test() -> TestInner {
        let (_control_sender, control_receiver) =
            TokioChannels::bounded::<ControlMessage<TestPayload, TokioChannels>, 4>();
        let (update_sender, _update_receiver) =
            TokioChannels::unbounded::<ClientUpdate<TestPayload>>();
        Inner {
            control_receiver,
            request_queue: Deque::new(),
            pending_responses: FnvIndexMap::new(),
            update_sender,
            interface: Ipv4Addr::LOCALHOST,
            discovery_socket: None,
            unicast_sockets: FnvIndexMap::new(),
            session_tracker: SessionTracker::default(),
            service_registry: ServiceRegistry::default(),
            run: true,
            client_id: 0x1234,
            session_counter: 1,
            sd_session_id: 1,
            sd_session_has_wrapped: false,
            e2e_registry: Arc::new(Mutex::new(E2ERegistry::new())),
            multicast_loopback: false,
            dispatch: crate::client::bind_dispatch::SpawnerDispatch {
                factory: TokioTransport,
                spawner: TokioSpawner,
            },
            timer: TokioTimer,
            phantom: core::marker::PhantomData,
        }
    }

    #[tokio::test]
    async fn bind_unicast_returns_capacity_error_when_map_full() {
        let mut inner = make_inner_for_test();

        // Fill unicast_sockets to capacity using ephemeral binds (port 0).
        // Each call with port=0 creates a fresh socket on a distinct OS-chosen
        // port, so the cap is what gates — not duplicate-key collapse.
        for _ in 0..UNICAST_SOCKETS_CAP {
            let bound = inner
                .bind_unicast(0)
                .await
                .expect("ephemeral bind below cap should succeed");
            assert_ne!(bound, 0, "OS should assign a non-zero ephemeral port");
        }
        assert_eq!(inner.unicast_sockets.len(), UNICAST_SOCKETS_CAP);

        // The next bind must fail with Error::Capacity and must NOT bind a
        // socket (pre-bind capacity check).
        let err = inner
            .bind_unicast(0)
            .await
            .expect_err("bind past cap should fail");
        match err {
            Error::Capacity(name) => assert_eq!(name, "unicast_sockets"),
            other => panic!("expected Error::Capacity, got {other:?}"),
        }
        assert_eq!(
            inner.unicast_sockets.len(),
            UNICAST_SOCKETS_CAP,
            "map should remain at capacity, not bind-then-drop a new socket"
        );
    }

    /// Happy path: with room in `pending_responses`, the helper tracks
    /// the entry and does NOT signal the caller — the sender stays
    /// alive so a future unicast reply can resolve it.
    #[tokio::test]
    async fn track_or_reject_pending_response_inserts_when_room_available() {
        use futures_util::FutureExt;
        let mut inner = make_inner_for_test();
        let (tx, rx) = oneshot::channel::<Result<TestPayload, Error>>();

        inner.track_or_reject_pending_response(0xDEAD_BEEF, tx);

        assert_eq!(inner.pending_responses.len(), 1);
        assert!(
            inner.pending_responses.contains_key(&0xDEAD_BEEF),
            "entry should be keyed by the provided request_id",
        );
        // Receiver is still waiting — helper did NOT pre-emptively
        // resolve it with a capacity error on the happy path.
        assert!(
            rx.now_or_never().is_none(),
            "receiver must still be pending when the insert succeeds",
        );
    }

    /// Regression guard against cb1d0d1: without explicit rejection,
    /// the dropped Sender would cause `PendingResponse::response()` to
    /// panic on `RecvError` rather than returning a clean
    /// `Err(Error::Capacity("pending_responses"))`. Exercises the
    /// overflow branch in `track_or_reject_pending_response`, which is
    /// the same branch the `SendToService` run-loop arm now delegates
    /// to.
    #[tokio::test]
    async fn track_or_reject_pending_response_rejects_on_saturation() {
        let mut inner = make_inner_for_test();

        // Fill the map to capacity with dummy oneshot senders. The
        // receivers are stashed to keep each channel open for the
        // remainder of the test — on `tokio::sync::oneshot`, dropping
        // the receiver does not drop the sender; it flips the sender
        // into a state where `send()` fails with the value returned.
        // The stash is what lets us later observe `sender.send(...)`
        // succeeding against a still-open channel when the overflow
        // case completes the displaced sender with a capacity error.
        let mut stashed: std::vec::Vec<oneshot::Receiver<Result<TestPayload, Error>>> =
            std::vec::Vec::with_capacity(PENDING_RESPONSES_CAP);
        for i in 0..PENDING_RESPONSES_CAP {
            let (tx, rx) = oneshot::channel::<Result<TestPayload, Error>>();
            inner
                .pending_responses
                .insert(
                    u32::try_from(i).expect("PENDING_RESPONSES_CAP fits in u32"),
                    tx,
                )
                .expect("filling under cap must succeed");
            stashed.push(rx);
        }
        assert_eq!(inner.pending_responses.len(), PENDING_RESPONSES_CAP);

        // One more entry — map is full, the helper must recover the
        // sender from the failed insert and deliver an explicit
        // capacity error on it.
        let (overflow_tx, overflow_rx) = oneshot::channel::<Result<TestPayload, Error>>();
        let overflow_key: u32 = 0xFFFF_FFFE;
        inner.track_or_reject_pending_response(overflow_key, overflow_tx);

        // Map size unchanged — the overflow attempt was rejected, not
        // silently dropping an existing entry.
        assert_eq!(
            inner.pending_responses.len(),
            PENDING_RESPONSES_CAP,
            "overflow must not evict existing entries",
        );
        assert!(
            !inner.pending_responses.contains_key(&overflow_key),
            "overflowed key must not be in the map",
        );

        // The caller's receiver resolves to Err(Capacity), not a
        // panicking RecvError — this is the invariant cb1d0d1 fixes.
        let result = overflow_rx
            .await
            .expect("receiver should get the explicit Err, not RecvError from dropped Sender");
        match result {
            Err(Error::Capacity(tag)) => assert_eq!(tag, "pending_responses"),
            other => panic!("expected Err(Error::Capacity(\"pending_responses\")), got {other:?}"),
        }
    }

    /// If a `request_id` is reused while an older pending entry is still
    /// live (e.g. `session_counter` wrap-around), `insert` returns
    /// `Ok(Some(old_sender))`. Without handling that case, the displaced
    /// sender is dropped and the caller awaiting the original request
    /// hits `RecvError` (which `PendingResponse::response()` treats as a
    /// fatal panic). This test guards against that: the displaced
    /// sender must be completed with
    /// `Err(Error::Capacity("pending_responses"))` so the original
    /// caller gets a clean `Result` instead of a panicking `RecvError`.
    #[tokio::test]
    async fn track_or_reject_pending_response_completes_displaced_sender() {
        use futures_util::FutureExt;

        let mut inner = make_inner_for_test();
        let key: u32 = 0xCAFE_F00D;

        // First tracking: the sender lives in the map.
        let (first_tx, first_rx) = oneshot::channel::<Result<TestPayload, Error>>();
        inner.track_or_reject_pending_response(key, first_tx);
        assert_eq!(inner.pending_responses.len(), 1);

        // Second tracking with the same key: displaces the first sender.
        let (second_tx, second_rx) = oneshot::channel::<Result<TestPayload, Error>>();
        inner.track_or_reject_pending_response(key, second_tx);

        // Map still has one entry — the second one replaced the first.
        assert_eq!(inner.pending_responses.len(), 1);
        assert!(inner.pending_responses.contains_key(&key));

        // The original caller's receiver resolves to Err(Capacity) — not
        // a dropped-sender RecvError.
        let displaced_result = first_rx.await.expect(
            "displaced sender must be completed with a real Err, \
             not dropped (which would produce RecvError)",
        );
        match displaced_result {
            Err(Error::Capacity(tag)) => assert_eq!(tag, "pending_responses"),
            other => {
                panic!("expected Err(Error::Capacity(\\\"pending_responses\\\")), got {other:?}")
            }
        }

        // The new sender is still live and pending.
        assert!(
            second_rx.now_or_never().is_none(),
            "replacement sender must still be pending in the map",
        );
    }

    /// Sibling to `client_new_with_spawner_routes_socket_spawns_through_it`
    /// in `mod.rs`, which covers the `bind_discovery` path. This one
    /// covers `bind_unicast`: each successful ephemeral unicast bind
    /// must submit exactly one future through the injected `Spawner`.
    /// Without this test, a future refactor could silently revert the
    /// unicast bind path to direct `tokio::spawn` and only the
    /// discovery path's test would fail to catch it.
    #[tokio::test]
    async fn bind_unicast_routes_through_injected_spawner() {
        use core::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Clone)]
        struct CountingSpawner {
            count: Arc<AtomicUsize>,
        }

        impl crate::transport::Spawner for CountingSpawner {
            fn spawn(&self, future: impl core::future::Future<Output = ()> + Send + 'static) {
                self.count.fetch_add(1, Ordering::SeqCst);
                // Delegate so the socket loop actually runs — matters
                // if the caller later issues a send that awaits the
                // loop's oneshot ack. For the pure-spawn-count
                // assertion below it would also work to drop the
                // future; we delegate to keep the Inner in a healthy
                // state in case assertion ordering changes.
                drop(tokio::spawn(future));
            }
        }

        let count = Arc::new(AtomicUsize::new(0));
        let spawner = CountingSpawner {
            count: Arc::clone(&count),
        };

        // Build Inner directly with the counting spawner — same pattern
        // as `make_inner_for_test`, but parameterized on S.
        let (_control_sender, control_receiver) = mpsc::channel(4);
        let (update_sender, _update_receiver) = mpsc::unbounded_channel();
        let mut inner: Inner<
            TestPayload,
            TokioTimer,
            Arc<Mutex<E2ERegistry>>,
            TokioChannels,
            crate::client::bind_dispatch::SpawnerDispatch<TokioTransport, CountingSpawner>,
        > = Inner {
            control_receiver,
            request_queue: Deque::new(),
            pending_responses: FnvIndexMap::new(),
            update_sender,
            interface: Ipv4Addr::LOCALHOST,
            discovery_socket: None,
            unicast_sockets: FnvIndexMap::new(),
            session_tracker: SessionTracker::default(),
            service_registry: ServiceRegistry::default(),
            run: true,
            client_id: 0x1234,
            session_counter: 1,
            sd_session_id: 1,
            sd_session_has_wrapped: false,
            e2e_registry: Arc::new(Mutex::new(E2ERegistry::new())),
            multicast_loopback: false,
            dispatch: crate::client::bind_dispatch::SpawnerDispatch {
                factory: TokioTransport,
                spawner,
            },
            timer: TokioTimer,
            phantom: core::marker::PhantomData,
        };

        // Three ephemeral binds → three distinct socket loops spawned.
        for i in 0..3 {
            let bound = inner
                .bind_unicast(0)
                .await
                .expect("ephemeral bind should succeed");
            assert_ne!(bound, 0, "iteration {i}: OS should assign a port");
        }

        assert_eq!(
            count.load(Ordering::SeqCst),
            3,
            "expected exactly three spawns (one per bind_unicast call), got {}",
            count.load(Ordering::SeqCst)
        );
    }

    #[tokio::test]
    async fn test_inner_build_and_shutdown() {
        let (control_sender, mut update_receiver, run_fut) = TestInner::build(
            Ipv4Addr::LOCALHOST,
            Arc::new(Mutex::new(E2ERegistry::new())),
            false,
            crate::client::bind_dispatch::SpawnerDispatch {
                factory: TokioTransport,
                spawner: TokioSpawner,
            },
            TokioTimer,
        );
        let _run_handle = tokio::spawn(run_fut);
        // Drop control sender to trigger loop exit
        drop(control_sender);
        // The update receiver should eventually return None when the inner loop exits
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            UnboundedRecv::recv(&mut update_receiver),
        )
        .await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    /// Helper: verify inner loop is still alive by sending an `AddEndpoint` and
    /// checking that a response arrives within 2 seconds.
    async fn assert_inner_alive(
        control_sender: &Sender<ControlMessage<TestPayload, TokioChannels>>,
    ) {
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9999);
        let (rx, msg) = TestControl::add_endpoint(0xFFFE, 0xFFFE, addr, 0);
        control_sender.send(msg).await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("Timed out — inner loop appears dead")
            .expect("Oneshot closed — inner loop appears dead");
        assert!(result.is_ok());
    }

    // -- Dropped-receiver robustness tests --
    // These verify that dropping the oneshot receiver before the inner loop
    // sends its response does NOT kill the processing loop (the `warn!`
    // paths that replaced `self.run = false`).

    #[tokio::test]
    async fn test_dropped_receiver_bind_discovery_continues() {
        let (control_sender, _update_receiver, run_fut) = TestInner::build(
            Ipv4Addr::LOCALHOST,
            Arc::new(Mutex::new(E2ERegistry::new())),
            false,
            crate::client::bind_dispatch::SpawnerDispatch {
                factory: TokioTransport,
                spawner: TokioSpawner,
            },
            TokioTimer,
        );
        let _run_handle = tokio::spawn(run_fut);

        let (rx, msg) = TestControl::bind_discovery();
        drop(rx);
        control_sender.send(msg).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        assert_inner_alive(&control_sender).await;
    }

    #[tokio::test]
    async fn test_dropped_receiver_unbind_discovery_continues() {
        let (control_sender, _update_receiver, run_fut) = TestInner::build(
            Ipv4Addr::LOCALHOST,
            Arc::new(Mutex::new(E2ERegistry::new())),
            false,
            crate::client::bind_dispatch::SpawnerDispatch {
                factory: TokioTransport,
                spawner: TokioSpawner,
            },
            TokioTimer,
        );
        let _run_handle = tokio::spawn(run_fut);

        let (rx, msg) = TestControl::unbind_discovery();
        drop(rx);
        control_sender.send(msg).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        assert_inner_alive(&control_sender).await;
    }

    #[tokio::test]
    async fn test_dropped_receiver_set_interface_continues() {
        let (control_sender, _update_receiver, run_fut) = TestInner::build(
            Ipv4Addr::LOCALHOST,
            Arc::new(Mutex::new(E2ERegistry::new())),
            false,
            crate::client::bind_dispatch::SpawnerDispatch {
                factory: TokioTransport,
                spawner: TokioSpawner,
            },
            TokioTimer,
        );
        let _run_handle = tokio::spawn(run_fut);

        // SetInterface(LOCALHOST) on a fresh inner goes straight to
        // bind_discovery + send response (interface already matches).
        let (rx, msg) = TestControl::set_interface(Ipv4Addr::LOCALHOST);
        drop(rx);
        control_sender.send(msg).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        assert_inner_alive(&control_sender).await;
    }

    #[tokio::test]
    async fn test_dropped_receiver_send_sd_continues() {
        let (control_sender, _update_receiver, run_fut) = TestInner::build(
            Ipv4Addr::LOCALHOST,
            Arc::new(Mutex::new(E2ERegistry::new())),
            false,
            crate::client::bind_dispatch::SpawnerDispatch {
                factory: TokioTransport,
                spawner: TokioSpawner,
            },
            TokioTimer,
        );
        let _run_handle = tokio::spawn(run_fut);

        // Bind discovery first so the SendSD path has a socket to use
        let (rx, msg) = TestControl::bind_discovery();
        control_sender.send(msg).await.unwrap();
        rx.recv().await.unwrap().unwrap();

        // Send SD with a dropped receiver
        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 30490);
        let sd_header = empty_sd_header();
        let (rx, msg) = TestControl::send_sd(target, sd_header);
        drop(rx);
        control_sender.send(msg).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        assert_inner_alive(&control_sender).await;
    }

    // -- Request queue test --
    // Verifies that when a new control message arrives while a multi-step
    // operation (SetInterface) is mid-way through processing, the new message
    // is queued and both complete successfully.

    #[tokio::test]
    async fn test_queued_messages_all_complete() {
        let (control_sender, _update_receiver, run_fut) = TestInner::build(
            Ipv4Addr::LOCALHOST,
            Arc::new(Mutex::new(E2ERegistry::new())),
            false,
            crate::client::bind_dispatch::SpawnerDispatch {
                factory: TokioTransport,
                spawner: TokioSpawner,
            },
            TokioTimer,
        );
        let _run_handle = tokio::spawn(run_fut);

        // Bind discovery so SetInterface will take the multi-step path:
        // iteration 1: unbind discovery, re-queue SetInterface
        // iteration 2: interface matches, bind discovery, send response
        let (rx, msg) = TestControl::bind_discovery();
        control_sender.send(msg).await.unwrap();
        rx.recv().await.unwrap().unwrap();

        // Queue both messages into the channel buffer before the inner loop
        // processes either. mpsc sends on a non-full buffer complete without
        // yielding, so both land before the spawned task runs.
        //
        // 1) SetInterface(LOCALHOST) — will unbind discovery, re-queue itself
        // 2) AddEndpoint — queued behind SetInterface, processed after it
        let (rx_set, msg_set) = TestControl::set_interface(Ipv4Addr::LOCALHOST);
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9999);
        let (rx_add, msg_add) = TestControl::add_endpoint(0x1234, 0x0001, addr, 0);
        control_sender.send(msg_set).await.unwrap();
        control_sender.send(msg_add).await.unwrap();

        // Both should complete successfully
        let set_result = tokio::time::timeout(std::time::Duration::from_secs(3), rx_set.recv())
            .await
            .expect("Timed out waiting for SetInterface")
            .expect("SetInterface oneshot closed");
        assert!(set_result.is_ok());

        let add_result = tokio::time::timeout(std::time::Duration::from_secs(3), rx_add.recv())
            .await
            .expect("Timed out waiting for AddEndpoint")
            .expect("AddEndpoint oneshot closed");
        assert!(add_result.is_ok());

        // Verify inner loop is still alive
        assert_inner_alive(&control_sender).await;
    }

    #[tokio::test]
    async fn test_send_to_service_constructor_returns_two_receivers() {
        let message = Message::<TestPayload>::new_sd(1, &empty_sd_header());
        let (send_rx, resp_rx, msg) = TestControl::send_to_service(0x1234, 0x0001, message);

        // Extract the senders from the control message
        if let ControlMessage::SendToService {
            send_complete,
            response,
            ..
        } = msg
        {
            // Both channels are independent — sending on one doesn't affect the other
            send_complete.send(Ok(())).unwrap();
            assert!(send_rx.recv().await.unwrap().is_ok());

            let payload = TestPayload {
                header: empty_sd_header(),
            };
            response.send(Ok(payload.clone())).unwrap();
            assert_eq!(resp_rx.recv().await.unwrap().unwrap(), payload);
        } else {
            panic!("expected SendToService variant");
        }
    }

    #[tokio::test]
    async fn test_dropped_receiver_add_endpoint_continues() {
        let (control_sender, _update_receiver, run_fut) = TestInner::build(
            Ipv4Addr::LOCALHOST,
            Arc::new(Mutex::new(E2ERegistry::new())),
            false,
            crate::client::bind_dispatch::SpawnerDispatch {
                factory: TokioTransport,
                spawner: TokioSpawner,
            },
            TokioTimer,
        );
        let _run_handle = tokio::spawn(run_fut);

        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5000);
        let (rx, msg) = TestControl::add_endpoint(0x1234, 0x0001, addr, 0);
        drop(rx);
        control_sender.send(msg).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        assert_inner_alive(&control_sender).await;
    }

    #[tokio::test]
    async fn test_dropped_receiver_remove_endpoint_continues() {
        let (control_sender, _update_receiver, run_fut) = TestInner::build(
            Ipv4Addr::LOCALHOST,
            Arc::new(Mutex::new(E2ERegistry::new())),
            false,
            crate::client::bind_dispatch::SpawnerDispatch {
                factory: TokioTransport,
                spawner: TokioSpawner,
            },
            TokioTimer,
        );
        let _run_handle = tokio::spawn(run_fut);

        let (rx, msg) = TestControl::remove_endpoint(0x1234, 0x0001);
        drop(rx);
        control_sender.send(msg).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        assert_inner_alive(&control_sender).await;
    }

    #[tokio::test]
    async fn test_dropped_receiver_send_to_service_send_complete_continues() {
        let (control_sender, _update_receiver, run_fut) = TestInner::build(
            Ipv4Addr::LOCALHOST,
            Arc::new(Mutex::new(E2ERegistry::new())),
            false,
            crate::client::bind_dispatch::SpawnerDispatch {
                factory: TokioTransport,
                spawner: TokioSpawner,
            },
            TokioTimer,
        );
        let _run_handle = tokio::spawn(run_fut);

        // Add an endpoint first so SendToService doesn't fail with ServiceNotFound
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5000);
        let (rx, msg) = TestControl::add_endpoint(0x1234, 0x0001, addr, 0);
        control_sender.send(msg).await.unwrap();
        rx.recv().await.unwrap().unwrap();

        // Send SendToService with the send_complete receiver dropped
        let message = Message::<TestPayload>::new_sd(1, &empty_sd_header());
        let (send_rx, _resp_rx, msg) = TestControl::send_to_service(0x1234, 0x0001, message);
        drop(send_rx);
        control_sender.send(msg).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        assert_inner_alive(&control_sender).await;
    }

    #[tokio::test]
    async fn test_bind_discovery_with_loopback() {
        // Spawn inner with multicast_loopback=true so bind_discovery exercises
        // the loopback-enabled branch of SocketManager::bind_discovery.
        let (control_sender, _update_receiver, run_fut) = TestInner::build(
            Ipv4Addr::LOCALHOST,
            Arc::new(Mutex::new(E2ERegistry::new())),
            true,
            crate::client::bind_dispatch::SpawnerDispatch {
                factory: TokioTransport,
                spawner: TokioSpawner,
            },
            TokioTimer,
        );
        let _run_handle = tokio::spawn(run_fut);

        let (rx, msg) = TestControl::bind_discovery();
        control_sender.send(msg).await.unwrap();
        rx.recv().await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_bind_discovery_idempotent() {
        // Binding discovery twice should succeed (early return on already-bound)
        let (control_sender, _update_receiver, run_fut) = TestInner::build(
            Ipv4Addr::LOCALHOST,
            Arc::new(Mutex::new(E2ERegistry::new())),
            false,
            crate::client::bind_dispatch::SpawnerDispatch {
                factory: TokioTransport,
                spawner: TokioSpawner,
            },
            TokioTimer,
        );
        let _run_handle = tokio::spawn(run_fut);

        let (rx, msg) = TestControl::bind_discovery();
        control_sender.send(msg).await.unwrap();
        rx.recv().await.unwrap().unwrap();

        // Second bind should also succeed (idempotent path)
        let (rx, msg) = TestControl::bind_discovery();
        control_sender.send(msg).await.unwrap();
        rx.recv().await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_send_sd_auto_binds_discovery() {
        // SendSD without a bound discovery socket should auto-bind and succeed
        let (control_sender, _update_receiver, run_fut) = TestInner::build(
            Ipv4Addr::LOCALHOST,
            Arc::new(Mutex::new(E2ERegistry::new())),
            false,
            crate::client::bind_dispatch::SpawnerDispatch {
                factory: TokioTransport,
                spawner: TokioSpawner,
            },
            TokioTimer,
        );
        let _run_handle = tokio::spawn(run_fut);

        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 30490);
        let sd_header = empty_sd_header();
        let (rx, msg) = TestControl::send_sd(target, sd_header);
        control_sender.send(msg).await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("Timed out waiting for SendSD")
            .expect("SendSD oneshot closed");
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_to_service_auto_binds_unicast() {
        // SendToService with no unicast sockets should auto-bind ephemeral
        let (control_sender, _update_receiver, run_fut) = TestInner::build(
            Ipv4Addr::LOCALHOST,
            Arc::new(Mutex::new(E2ERegistry::new())),
            false,
            crate::client::bind_dispatch::SpawnerDispatch {
                factory: TokioTransport,
                spawner: TokioSpawner,
            },
            TokioTimer,
        );
        let _run_handle = tokio::spawn(run_fut);

        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5000);
        let (rx, msg) = TestControl::add_endpoint(0x1234, 0x0001, addr, 0);
        control_sender.send(msg).await.unwrap();
        rx.recv().await.unwrap().unwrap();

        let message = Message::<TestPayload>::new_sd(1, &empty_sd_header());
        let (send_rx, _resp_rx, msg) = TestControl::send_to_service(0x1234, 0x0001, message);
        control_sender.send(msg).await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), send_rx.recv())
            .await
            .expect("Timed out waiting for SendToService")
            .expect("SendToService oneshot closed");
        assert!(result.is_ok(), "send should succeed: {result:?}");
    }

    #[tokio::test]
    async fn test_subscribe_with_endpoint_sends_sd() {
        // Subscribe with a known endpoint and bound discovery should send the SD message
        let (control_sender, _update_receiver, run_fut) = TestInner::build(
            Ipv4Addr::LOCALHOST,
            Arc::new(Mutex::new(E2ERegistry::new())),
            false,
            crate::client::bind_dispatch::SpawnerDispatch {
                factory: TokioTransport,
                spawner: TokioSpawner,
            },
            TokioTimer,
        );
        let _run_handle = tokio::spawn(run_fut);

        // Bind discovery first
        let (rx, msg) = TestControl::bind_discovery();
        control_sender.send(msg).await.unwrap();
        rx.recv().await.unwrap().unwrap();

        // Add endpoint
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5000);
        let (rx, msg) = TestControl::add_endpoint(0x1234, 0x0001, addr, 0);
        control_sender.send(msg).await.unwrap();
        rx.recv().await.unwrap().unwrap();

        // Subscribe
        let (rx, msg) = TestControl::subscribe(0x1234, 0x0001, 1, 3, 0x01, 0);
        control_sender.send(msg).await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("Timed out waiting for Subscribe")
            .expect("Subscribe oneshot closed");
        assert!(result.is_ok(), "subscribe should succeed: {result:?}");
    }

    #[tokio::test]
    async fn test_subscribe_auto_binds_discovery() {
        // Subscribe without discovery bound should auto-bind and succeed
        let (control_sender, _update_receiver, run_fut) = TestInner::build(
            Ipv4Addr::LOCALHOST,
            Arc::new(Mutex::new(E2ERegistry::new())),
            false,
            crate::client::bind_dispatch::SpawnerDispatch {
                factory: TokioTransport,
                spawner: TokioSpawner,
            },
            TokioTimer,
        );
        let _run_handle = tokio::spawn(run_fut);

        // Add endpoint but do NOT bind discovery
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5000);
        let (rx, msg) = TestControl::add_endpoint(0x1234, 0x0001, addr, 0);
        control_sender.send(msg).await.unwrap();
        rx.recv().await.unwrap().unwrap();

        // Subscribe should auto-bind discovery
        let (rx, msg) = TestControl::subscribe(0x1234, 0x0001, 1, 3, 0x01, 0);
        control_sender.send(msg).await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("Timed out waiting for Subscribe")
            .expect("Subscribe oneshot closed");
        assert!(result.is_ok(), "subscribe should auto-bind: {result:?}");
    }

    #[tokio::test]
    async fn test_subscribe_unknown_service_returns_error() {
        let (control_sender, _update_receiver, run_fut) = TestInner::build(
            Ipv4Addr::LOCALHOST,
            Arc::new(Mutex::new(E2ERegistry::new())),
            false,
            crate::client::bind_dispatch::SpawnerDispatch {
                factory: TokioTransport,
                spawner: TokioSpawner,
            },
            TokioTimer,
        );
        let _run_handle = tokio::spawn(run_fut);

        let (rx, msg) = TestControl::subscribe(0xFFFF, 0xFFFF, 1, 3, 0x01, 0);
        control_sender.send(msg).await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("Timed out")
            .expect("oneshot closed");
        assert!(matches!(result, Err(Error::ServiceNotFound)));
    }

    #[tokio::test]
    async fn test_send_to_service_reuses_existing_unicast_socket() {
        // When a unicast socket already exists, SendToService should reuse it
        let (control_sender, _update_receiver, run_fut) = TestInner::build(
            Ipv4Addr::LOCALHOST,
            Arc::new(Mutex::new(E2ERegistry::new())),
            false,
            crate::client::bind_dispatch::SpawnerDispatch {
                factory: TokioTransport,
                spawner: TokioSpawner,
            },
            TokioTimer,
        );
        let _run_handle = tokio::spawn(run_fut);

        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5000);
        let (rx, msg) = TestControl::add_endpoint(0x1234, 0x0001, addr, 0);
        control_sender.send(msg).await.unwrap();
        rx.recv().await.unwrap().unwrap();

        // First send auto-binds unicast
        let message = Message::<TestPayload>::new_sd(1, &empty_sd_header());
        let (send_rx, _resp_rx, msg) = TestControl::send_to_service(0x1234, 0x0001, message);
        control_sender.send(msg).await.unwrap();
        send_rx.recv().await.unwrap().unwrap();

        // Second send reuses the existing socket (no auto-bind needed)
        let message = Message::<TestPayload>::new_sd(1, &empty_sd_header());
        let (send_rx, _resp_rx, msg) = TestControl::send_to_service(0x1234, 0x0001, message);
        control_sender.send(msg).await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), send_rx.recv())
            .await
            .expect("Timed out")
            .expect("oneshot closed");
        assert!(
            result.is_ok(),
            "second send should reuse socket: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_dropped_receiver_subscribe_service_not_found_continues() {
        // Subscribe with no endpoint → ServiceNotFound response is dropped
        let (control_sender, _update_receiver, run_fut) = TestInner::build(
            Ipv4Addr::LOCALHOST,
            Arc::new(Mutex::new(E2ERegistry::new())),
            false,
            crate::client::bind_dispatch::SpawnerDispatch {
                factory: TokioTransport,
                spawner: TokioSpawner,
            },
            TokioTimer,
        );
        let _run_handle = tokio::spawn(run_fut);

        let (rx, msg) = TestControl::subscribe(0x1234, 0x0001, 1, 3, 0x01, 0);
        drop(rx);
        control_sender.send(msg).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        assert_inner_alive(&control_sender).await;
    }

    #[tokio::test]
    async fn test_set_interface_changes_interface() {
        // SetInterface to a different address exercises the interface!=current path
        let (control_sender, _update_receiver, run_fut) = TestInner::build(
            Ipv4Addr::LOCALHOST,
            Arc::new(Mutex::new(E2ERegistry::new())),
            false,
            crate::client::bind_dispatch::SpawnerDispatch {
                factory: TokioTransport,
                spawner: TokioSpawner,
            },
            TokioTimer,
        );
        let _run_handle = tokio::spawn(run_fut);

        // Change to a different loopback-range address (127.0.0.2).
        // Binding discovery on 127.0.0.2 should succeed on most systems.
        let (rx, msg) = TestControl::set_interface(Ipv4Addr::new(127, 0, 0, 2));
        control_sender.send(msg).await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
            .await
            .expect("Timed out waiting for SetInterface")
            .expect("SetInterface oneshot closed");
        // The result may be Ok or Err depending on whether 127.0.0.2 is bindable,
        // but the important thing is that the inner loop didn't panic or deadlock.
        let _ = result;

        assert_inner_alive(&control_sender).await;
    }

    #[tokio::test]
    async fn test_set_interface_with_discovery_bound_changes_interface() {
        // SetInterface when discovery is already bound: unbind → change → rebind
        let (control_sender, _update_receiver, run_fut) = TestInner::build(
            Ipv4Addr::LOCALHOST,
            Arc::new(Mutex::new(E2ERegistry::new())),
            false,
            crate::client::bind_dispatch::SpawnerDispatch {
                factory: TokioTransport,
                spawner: TokioSpawner,
            },
            TokioTimer,
        );
        let _run_handle = tokio::spawn(run_fut);

        // Bind discovery on LOCALHOST first
        let (rx, msg) = TestControl::bind_discovery();
        control_sender.send(msg).await.unwrap();
        rx.recv().await.unwrap().unwrap();

        // Change to 127.0.0.2 — this takes the multi-step path:
        // 1. unbind discovery, re-queue
        // 2. interface != 127.0.0.2, set_interface, re-queue
        // 3. interface == 127.0.0.2, bind discovery
        let (rx, msg) = TestControl::set_interface(Ipv4Addr::new(127, 0, 0, 2));
        control_sender.send(msg).await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
            .await
            .expect("Timed out waiting for SetInterface")
            .expect("SetInterface oneshot closed");
        let _ = result;

        assert_inner_alive(&control_sender).await;
    }

    #[tokio::test]
    async fn test_subscribe_specific_port_reuse() {
        // Subscribe twice with the same specific client_port exercises the
        // bind_unicast port-reuse path (port != 0 && already bound).
        let (control_sender, _update_receiver, run_fut) = TestInner::build(
            Ipv4Addr::LOCALHOST,
            Arc::new(Mutex::new(E2ERegistry::new())),
            false,
            crate::client::bind_dispatch::SpawnerDispatch {
                factory: TokioTransport,
                spawner: TokioSpawner,
            },
            TokioTimer,
        );
        let _run_handle = tokio::spawn(run_fut);

        // Add endpoint and bind discovery
        let (rx, msg) = TestControl::bind_discovery();
        control_sender.send(msg).await.unwrap();
        rx.recv().await.unwrap().unwrap();

        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5000);
        let (rx, msg) = TestControl::add_endpoint(0x1234, 0x0001, addr, 0);
        control_sender.send(msg).await.unwrap();
        rx.recv().await.unwrap().unwrap();

        // First subscribe with specific port — binds the port
        let (rx, msg) = TestControl::subscribe(0x1234, 0x0001, 1, 3, 0x01, 44444);
        control_sender.send(msg).await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("Timed out")
            .expect("oneshot closed");
        assert!(result.is_ok(), "first subscribe should succeed: {result:?}");

        // Second subscribe with the same port — reuses the existing socket
        let (rx, msg) = TestControl::subscribe(0x1234, 0x0001, 1, 3, 0x02, 44444);
        control_sender.send(msg).await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("Timed out")
            .expect("oneshot closed");
        assert!(
            result.is_ok(),
            "second subscribe should reuse port: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_sd_session_id_persists_across_rebind() {
        // Verify that unbind_discovery + bind_discovery carries the session counter
        // forward rather than resetting it to 1, which would send a false reboot signal.
        use crate::protocol::MessageView;
        use std::vec;
        use tokio::net::UdpSocket;

        let (control_sender, _update_receiver, run_fut) = TestInner::build(
            Ipv4Addr::LOCALHOST,
            Arc::new(Mutex::new(E2ERegistry::new())),
            false,
            crate::client::bind_dispatch::SpawnerDispatch {
                factory: TokioTransport,
                spawner: TokioSpawner,
            },
            TokioTimer,
        );
        let _run_handle = tokio::spawn(run_fut);

        let raw = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, raw.local_addr().unwrap().port());

        // Bind and send one SD message to advance the session counter.
        let (rx, msg) = TestControl::bind_discovery();
        control_sender.send(msg).await.unwrap();
        rx.recv().await.unwrap().unwrap();

        let (rx, msg) = TestControl::send_sd(target, empty_sd_header());
        control_sender.send(msg).await.unwrap();
        rx.recv().await.unwrap().unwrap();

        let mut buf = vec![0u8; 1400];
        let (len, _) =
            tokio::time::timeout(std::time::Duration::from_secs(2), raw.recv_from(&mut buf))
                .await
                .expect("timed out waiting for first SD message")
                .unwrap();
        let first = MessageView::parse(&buf[..len]).unwrap();
        let session_id_before = (first.header().request_id() & 0xFFFF) as u16;
        let reboot_flag_before = first.sd_header().unwrap().flags().reboot();
        assert!(session_id_before >= 1, "session_id must never be 0");

        // Unbind, then rebind.
        let (rx, msg) = TestControl::unbind_discovery();
        control_sender.send(msg).await.unwrap();
        rx.recv().await.unwrap().unwrap();

        let (rx, msg) = TestControl::bind_discovery();
        control_sender.send(msg).await.unwrap();
        rx.recv().await.unwrap().unwrap();

        // Send a second SD message and verify both session counter and reboot flag persisted.
        let (rx, msg) = TestControl::send_sd(target, empty_sd_header());
        control_sender.send(msg).await.unwrap();
        rx.recv().await.unwrap().unwrap();

        let (len, _) =
            tokio::time::timeout(std::time::Duration::from_secs(2), raw.recv_from(&mut buf))
                .await
                .expect("timed out waiting for second SD message")
                .unwrap();
        let second = MessageView::parse(&buf[..len]).unwrap();
        let session_id_after = (second.header().request_id() & 0xFFFF) as u16;
        let reboot_flag_after = second.sd_header().unwrap().flags().reboot();

        assert!(
            session_id_after > session_id_before,
            "session_id should continue after rebind (before={session_id_before}, after={session_id_after})"
        );
        assert_eq!(
            reboot_flag_after, reboot_flag_before,
            "reboot_flag should be preserved across rebind"
        );
    }
}
