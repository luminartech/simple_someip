use std::{
    borrow::ToOwned,
    collections::HashMap,
    future,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{Arc, Mutex},
    task::Poll,
};
use tokio::{
    select,
    sync::{
        mpsc::{self, Receiver, Sender},
        oneshot,
    },
};
use tracing::{debug, error, info, trace, warn};

use crate::{
    client::{
        ClientUpdate, DiscoveryMessage,
        service_registry::{ServiceEndpointInfo, ServiceInstanceId, ServiceRegistry},
        session::{SessionTracker, SessionVerdict, TransportKind},
        socket_manager::{ReceivedMessage, SocketManager},
    },
    e2e::E2ERegistry,
    protocol::{self, Message},
    traits::PayloadWireFormat,
};

use super::error::Error;

pub(super) enum ControlMessage<P: PayloadWireFormat> {
    SetInterface(Ipv4Addr, oneshot::Sender<Result<(), Error>>),
    BindDiscovery(oneshot::Sender<Result<(), Error>>),
    UnbindDiscovery(oneshot::Sender<Result<(), Error>>),
    SendSD(
        SocketAddrV4,
        P::SdHeader,
        oneshot::Sender<Result<(), Error>>,
    ),
    AddEndpoint(u16, u16, SocketAddrV4, oneshot::Sender<Result<(), Error>>),
    RemoveEndpoint(u16, u16, oneshot::Sender<Result<(), Error>>),
    SendToService {
        service_id: u16,
        instance_id: u16,
        message: Message<P>,
        /// Fires when the UDP send completes (or errors on lookup/bind).
        send_complete: oneshot::Sender<Result<(), Error>>,
        /// Fires when a matching unicast response arrives.
        response: oneshot::Sender<Result<P, Error>>,
    },
    Subscribe {
        service_id: u16,
        instance_id: u16,
        major_version: u8,
        ttl: u32,
        event_group_id: u16,
        client_port: u16,
        response: oneshot::Sender<Result<(), Error>>,
    },
}

impl<P: PayloadWireFormat> std::fmt::Debug for ControlMessage<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SetInterface(addr, _) => f.debug_tuple("SetInterface").field(addr).finish(),
            Self::BindDiscovery(_) => f.write_str("BindDiscovery"),
            Self::UnbindDiscovery(_) => f.write_str("UnbindDiscovery"),
            Self::SendSD(addr, header, _) => {
                f.debug_tuple("SendSD").field(addr).field(header).finish()
            }
            Self::AddEndpoint(sid, iid, addr, _) => f
                .debug_tuple("AddEndpoint")
                .field(sid)
                .field(iid)
                .field(addr)
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
        }
    }
}

impl<P: PayloadWireFormat> ControlMessage<P> {
    pub fn set_interface(interface: Ipv4Addr) -> (oneshot::Receiver<Result<(), Error>>, Self) {
        let (sender, receiver) = oneshot::channel();
        (receiver, Self::SetInterface(interface, sender))
    }
    pub fn bind_discovery() -> (oneshot::Receiver<Result<(), Error>>, Self) {
        let (sender, receiver) = oneshot::channel();
        (receiver, Self::BindDiscovery(sender))
    }
    pub fn unbind_discovery() -> (oneshot::Receiver<Result<(), Error>>, Self) {
        let (sender, receiver) = oneshot::channel();
        (receiver, Self::UnbindDiscovery(sender))
    }

    pub fn send_sd(
        socket_addr: SocketAddrV4,
        header: P::SdHeader,
    ) -> (oneshot::Receiver<Result<(), Error>>, Self) {
        let (sender, receiver) = oneshot::channel();
        (receiver, Self::SendSD(socket_addr, header, sender))
    }
    pub fn add_endpoint(
        service_id: u16,
        instance_id: u16,
        addr: SocketAddrV4,
    ) -> (oneshot::Receiver<Result<(), Error>>, Self) {
        let (sender, receiver) = oneshot::channel();
        (
            receiver,
            Self::AddEndpoint(service_id, instance_id, addr, sender),
        )
    }

    pub fn remove_endpoint(
        service_id: u16,
        instance_id: u16,
    ) -> (oneshot::Receiver<Result<(), Error>>, Self) {
        let (sender, receiver) = oneshot::channel();
        (
            receiver,
            Self::RemoveEndpoint(service_id, instance_id, sender),
        )
    }

    #[allow(clippy::type_complexity)]
    pub fn send_to_service(
        service_id: u16,
        instance_id: u16,
        message: Message<P>,
    ) -> (
        oneshot::Receiver<Result<(), Error>>,
        oneshot::Receiver<Result<P, Error>>,
        Self,
    ) {
        let (send_complete_tx, send_complete_rx) = oneshot::channel();
        let (response_tx, response_rx) = oneshot::channel();
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

    pub fn subscribe(
        service_id: u16,
        instance_id: u16,
        major_version: u8,
        ttl: u32,
        event_group_id: u16,
        client_port: u16,
    ) -> (oneshot::Receiver<Result<(), Error>>, Self) {
        let (sender, receiver) = oneshot::channel();
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
}

pub(super) struct Inner<PayloadDefinitions: PayloadWireFormat> {
    /// MPSC Receiver used to receive control messages from outer client
    control_receiver: Receiver<ControlMessage<PayloadDefinitions>>,
    /// The active request, if one is being served
    active_request: Option<ControlMessage<PayloadDefinitions>>,
    /// Pending request-response: (`message_id`, `response_sender`).
    /// Set by `SendToService`, cleared when a matching unicast arrives or sender is dropped.
    pending_response: Option<(
        protocol::MessageId,
        oneshot::Sender<Result<PayloadDefinitions, Error>>,
    )>,
    /// MPSC Sender used to send updates to outer client
    update_sender: mpsc::Sender<ClientUpdate<PayloadDefinitions>>,
    /// Target interface for sockets
    interface: Ipv4Addr,
    /// Socket manager for service discovery if bound
    discovery_socket: Option<SocketManager<PayloadDefinitions>>,
    /// Socket managers for unicast messages, keyed by local port
    unicast_sockets: HashMap<u16, SocketManager<PayloadDefinitions>>,
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
    /// Shared E2E registry for runtime E2E configuration
    e2e_registry: Arc<Mutex<E2ERegistry>>,
    /// Phantom data to represent the generic message definitions
    phantom: std::marker::PhantomData<PayloadDefinitions>,
}

impl<PayloadDefinitions: PayloadWireFormat> std::fmt::Debug for Inner<PayloadDefinitions> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("interface", &self.interface)
            .field("session_tracker", &self.session_tracker)
            .field("run", &self.run)
            .field("client_id", &self.client_id)
            .field("session_counter", &self.session_counter)
            .finish_non_exhaustive()
    }
}

impl<PayloadDefinitions> Inner<PayloadDefinitions>
where
    PayloadDefinitions: PayloadWireFormat + Clone + std::fmt::Debug + 'static,
{
    pub fn spawn(
        interface: Ipv4Addr,
        e2e_registry: Arc<Mutex<E2ERegistry>>,
    ) -> (
        Sender<ControlMessage<PayloadDefinitions>>,
        Receiver<ClientUpdate<PayloadDefinitions>>,
    ) {
        info!("Initializing SOME/IP Client");
        let (control_sender, control_receiver) = mpsc::channel(4);
        let (update_sender, update_receiver) = mpsc::channel(4);
        let inner = Self {
            control_receiver,
            active_request: None,
            pending_response: None,
            update_sender,
            interface,
            discovery_socket: None,
            unicast_sockets: HashMap::new(),
            session_tracker: SessionTracker::default(),
            service_registry: ServiceRegistry::default(),
            run: true,
            client_id: 0x1234,
            session_counter: 1,
            e2e_registry,
            phantom: std::marker::PhantomData,
        };
        inner.run();
        (control_sender, update_receiver)
    }

    fn bind_discovery(&mut self) -> Result<(), Error> {
        if self.discovery_socket.is_some() {
            Ok(())
        } else {
            let socket =
                SocketManager::bind_discovery(self.interface, Arc::clone(&self.e2e_registry))?;
            self.discovery_socket = Some(socket);
            Ok(())
        }
    }

    // Dropping the receiver kills the loop
    async fn unbind_discovery(&mut self) {
        debug!("Unbinding Discovery socket.");
        if let Some(socket_manger) = self.discovery_socket.take() {
            socket_manger.shut_down().await;
        }
    }

    fn set_interface(&mut self, interface: Ipv4Addr) {
        self.interface = interface;
    }

    fn bind_unicast(&mut self, port: u16) -> Result<u16, Error> {
        if port != 0
            && let Some(socket) = self.unicast_sockets.get(&port)
        {
            return Ok(socket.port());
        }
        let unicast_socket = SocketManager::bind(port, Arc::clone(&self.e2e_registry))?;
        let bound_port = unicast_socket.port();
        self.unicast_sockets.insert(bound_port, unicast_socket);
        debug!("Bound unicast socket on port {}", bound_port);
        Ok(bound_port)
    }

    async fn receive_discovery(
        socket_manager: &mut Option<SocketManager<PayloadDefinitions>>,
    ) -> Result<
        (
            SocketAddr,
            protocol::Header,
            <PayloadDefinitions as PayloadWireFormat>::SdHeader,
        ),
        Error,
    > {
        if let Some(receiver) = socket_manager {
            match receiver.receive().await {
                Some(result) => match result {
                    Ok(received) => {
                        let someip_header = received.message.header().clone();
                        if let Some(sd_header) = received.message.sd_header() {
                            Ok((received.source, someip_header, sd_header.to_owned()))
                        } else {
                            Err(Error::UnexpectedDiscoveryMessage(someip_header))
                        }
                    }
                    Err(err) => Err(err),
                },
                None => Err(Error::SocketClosedUnexpectedly),
            }
        } else {
            // If we don't have a receiver, we should return a future that never resolves
            future::pending().await
        }
    }

    /// Receive from any bound unicast socket. Returns the first message ready
    /// from any socket. If no sockets are bound, returns a future that never resolves.
    async fn receive_any_unicast(
        unicast_sockets: &mut HashMap<u16, SocketManager<PayloadDefinitions>>,
    ) -> Result<ReceivedMessage<PayloadDefinitions>, Error> {
        if unicast_sockets.is_empty() {
            return future::pending().await;
        }

        // Use poll_fn to manually poll each socket's receiver
        std::future::poll_fn(|cx| {
            for socket in unicast_sockets.values_mut() {
                if let Poll::Ready(result) = socket.poll_receive(cx) {
                    return Poll::Ready(match result {
                        Some(msg) => msg,
                        None => Err(Error::SocketClosedUnexpectedly),
                    });
                }
            }
            Poll::Pending
        })
        .await
    }

    #[allow(clippy::too_many_lines)]
    async fn handle_control_message(&mut self) {
        if let Some(active_request) = self.active_request.take() {
            match active_request {
                ControlMessage::SetInterface(interface, response) => {
                    if self.discovery_socket.is_some() {
                        info!(
                            "Discovery socket currently bound to interface: {}, unbinding.",
                            self.interface
                        );
                        self.unbind_discovery().await;
                        self.active_request =
                            Some(ControlMessage::SetInterface(interface, response));
                        return;
                    }
                    if self.interface != interface {
                        self.set_interface(interface);
                        self.active_request =
                            Some(ControlMessage::SetInterface(interface, response));
                        return;
                    }
                    info!("Binding to interface: {}", interface);
                    let bind_result = self.bind_discovery();
                    match &bind_result {
                        Ok(()) => {
                            info!("Successfully Bound to interface: {}", interface);
                        }
                        Err(e) => {
                            warn!("Failed to bind to interface: {}. Error: {:?}", interface, e);
                        }
                    }
                    if response.send(bind_result).is_err() {
                        warn!("SetInterface response receiver dropped (caller canceled)");
                    }
                }
                ControlMessage::BindDiscovery(response) => {
                    let result = self.bind_discovery();
                    if response.send(result).is_err() {
                        warn!("BindDiscovery response receiver dropped (caller canceled)");
                    }
                }
                ControlMessage::UnbindDiscovery(response) => {
                    self.unbind_discovery().await;
                    if response.send(Ok(())).is_err() {
                        warn!("UnbindDiscovery response receiver dropped (caller canceled)");
                    }
                }
                ControlMessage::SendSD(target, header, response) => {
                    // SD Message, If the discovery socket is not bound, bind it
                    match &mut self.discovery_socket {
                        None => {
                            match self.bind_discovery() {
                                Ok(()) => {
                                    // Discovery socket successfully bound, send the message on the next loop
                                    self.active_request =
                                        Some(ControlMessage::SendSD(target, header, response));
                                }
                                Err(e) => {
                                    error!(
                                        "Failed to bind discovery socket for sending SD message: {:?}",
                                        e
                                    );
                                    if response.send(Err(e)).is_err() {
                                        warn!(
                                            "SendSD error response receiver dropped (caller canceled)"
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
                                warn!("SendSD response receiver dropped (caller canceled)");
                            }
                        }
                    }
                }
                ControlMessage::AddEndpoint(service_id, instance_id, addr, response) => {
                    self.service_registry.insert(
                        ServiceInstanceId {
                            service_id,
                            instance_id,
                        },
                        ServiceEndpointInfo {
                            addr,
                            major_version: 0xFF,
                            minor_version: 0xFFFF_FFFF,
                        },
                    );
                    debug!(
                        "Added endpoint for service 0x{:04X}.0x{:04X} -> {}",
                        service_id, instance_id, addr,
                    );
                    if response.send(Ok(())).is_err() {
                        warn!("AddEndpoint response receiver dropped (caller canceled)");
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
                        warn!("RemoveEndpoint response receiver dropped (caller canceled)");
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

                    // Auto-bind unicast if no sockets exist
                    if self.unicast_sockets.is_empty() {
                        match self.bind_unicast(0) {
                            Ok(port) => {
                                debug!("Auto-bound unicast on port {} for SendToService", port);
                            }
                            Err(e) => {
                                let _ = send_complete.send(Err(e));
                                return;
                            }
                        }
                    }

                    // Use the first available unicast socket
                    let source_port = *self.unicast_sockets.keys().next().unwrap();
                    let socket = self.unicast_sockets.get_mut(&source_port).unwrap();

                    // Stamp request ID
                    let request_id =
                        (u32::from(self.client_id) << 16) | u32::from(self.session_counter);
                    message.set_request_id(request_id);
                    self.session_counter = self.session_counter.wrapping_add(1);
                    if self.session_counter == 0 {
                        self.session_counter = 1;
                    }

                    let message_id = message.header().message_id();
                    let send_result = socket.send(target, message).await;
                    match send_result {
                        Ok(()) => {
                            let _ = send_complete.send(Ok(()));
                            // Drop any prior pending response (caller gets RecvError)
                            self.pending_response = Some((message_id, response));
                        }
                        Err(e) => {
                            let _ = send_complete.send(Err(e));
                        }
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
                        let _ = response.send(Err(Error::ServiceNotFound));
                        return;
                    }

                    // Bind unicast on the requested port (0 = ephemeral)
                    let unicast_port = match self.bind_unicast(client_port) {
                        Ok(port) => {
                            debug!("Bound unicast on port {} for Subscribe", port);
                            port
                        }
                        Err(e) => {
                            let _ = response.send(Err(e));
                            return;
                        }
                    };

                    // Auto-bind discovery if not bound (re-queue like SendSD does)
                    match &mut self.discovery_socket {
                        None => match self.bind_discovery() {
                            Ok(()) => {
                                self.active_request = Some(ControlMessage::Subscribe {
                                    service_id,
                                    instance_id,
                                    major_version,
                                    ttl,
                                    event_group_id,
                                    client_port,
                                    response,
                                });
                            }
                            Err(e) => {
                                let _ = response.send(Err(e));
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
                                warn!("Subscribe response receiver dropped (caller canceled)");
                            }
                        }
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn run(mut self) {
        tokio::spawn(async move {
            info!("SOME/IP Client processing loop started");
            loop {
                let Self {
                    control_receiver,
                    pending_response,
                    discovery_socket,
                    unicast_sockets,
                    update_sender,
                    active_request,
                    session_tracker,
                    service_registry,
                    run,
                    ..
                } = &mut self;
                select! {
                    () = tokio::time::sleep(std::time::Duration::from_millis(125)) => {}
                    // Receive a control message
                    ctrl = control_receiver.recv() => {
                        if let Some(ctrl) = ctrl {
                            if active_request.is_some() {
                                // Multi-step operations (e.g. SetInterface, SendSD)
                                // re-queue themselves across loop iterations. Do not
                                // discard mid-operation. Let the existing request finish.
                                error!(
                                    "Received new control message while active_request \
                                     is in progress; ignoring new message"
                                );
                                continue;
                            }
                            debug!("Received control message: {:?}", ctrl);
                            *active_request = Some(ctrl);
                        } else {
                            // The sender has been dropped, so we should exit
                            *run = false;
                        }
                    }
                    // Receive a discovery message
                    discovery = Inner::receive_discovery(discovery_socket) => {
                        trace!("Received discovery message: {:?}", discovery);
                        match discovery {
                            Ok((source, someip_header, sd_header)) => {
                                // Extract session ID from SOME/IP request_id (lower 16 bits)
                                let session_id = (someip_header.request_id() & 0xFFFF) as u16;
                                // Extract reboot flag from the SD payload flags
                                let reboot_flag = PayloadDefinitions::new_sd_payload(&sd_header)
                                    .sd_flags()
                                    .is_some_and(crate::protocol::sd::Flags::reboot);
                                let verdict = session_tracker.check(
                                    source,
                                    TransportKind::Multicast,
                                    session_id,
                                    reboot_flag,
                                );
                                if verdict == SessionVerdict::Reboot
                                    && update_sender.send(ClientUpdate::SenderRebooted(source)).await.is_err()
                                {
                                    *run = false;
                                    continue;
                                }

                                // Auto-populate service registry from SD entries
                                let sd_payload = PayloadDefinitions::new_sd_payload(&sd_header);
                                for ep in sd_payload.offered_endpoints() {
                                    let id = ServiceInstanceId {
                                        service_id: ep.service_id,
                                        instance_id: ep.instance_id,
                                    };
                                    if ep.is_offer {
                                        if let Some(addr) = ep.addr {
                                            service_registry.insert(
                                                id,
                                                ServiceEndpointInfo {
                                                    addr,
                                                    major_version: ep.major_version,
                                                    minor_version: ep.minor_version,
                                                },
                                            );
                                            trace!(
                                                "Registry: added 0x{:04X}.0x{:04X} -> {}",
                                                ep.service_id, ep.instance_id, addr,
                                            );
                                        }
                                    } else {
                                        service_registry.remove(id);
                                        trace!(
                                            "Registry: removed 0x{:04X}.0x{:04X}",
                                            ep.service_id, ep.instance_id,
                                        );
                                    }
                                }

                                let discovery_msg = DiscoveryMessage {
                                    source,
                                    someip_header,
                                    sd_header,
                                };
                                if update_sender.send(ClientUpdate::DiscoveryUpdated(discovery_msg)).await.is_err() {
                                    *run = false;
                                }
                            }
                            Err(err) => {
                                error!("Error receiving discovery message: {:?}", err);
                                if update_sender.send(ClientUpdate::Error(err)).await.is_err() {
                                    // The sender has been dropped, so we should exit
                                    *run = false;
                                }
                            }
                        }
                     }
                     unicast = Inner::receive_any_unicast(unicast_sockets) => {
                         trace!("Received unicast message: {:?}", unicast);
                         match unicast {
                             Ok(received) => {
                                 let ReceivedMessage { message: received_message, e2e_status, .. } = received;
                                 // Check if this matches a pending request-response
                                 if let Some((pending_id, _)) = pending_response
                                     && *pending_id == received_message.header().message_id()
                                 {
                                     let (_, sender) = pending_response.take().unwrap();
                                     let _ = sender.send(Ok(received_message.payload().clone()));
                                     continue;
                                 }
                                 // Not a response — forward as ClientUpdate::Unicast
                                 if update_sender.send(ClientUpdate::Unicast { message: received_message, e2e_status }).await.is_err() {
                                     *run = false;
                                 }
                             }
                             Err(err) => {
                                 if update_sender.send(ClientUpdate::Error(err)).await.is_err() {
                                     // The sender has been dropped, so we should exit
                                     *run = false;
                                 }
                             }
                         }
                     }
                }
                if !*run {
                    info!("SOME/IP Client processing loop exiting");
                    break;
                }
                self.handle_control_message().await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::sd::test_support::{TestPayload, empty_sd_header};
    use std::format;

    type TestControl = ControlMessage<TestPayload>;

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
        let (_rx, msg) = TestControl::add_endpoint(0x1234, 0x0001, addr);
        assert!(matches!(msg, ControlMessage::AddEndpoint(..)));

        let (_rx, msg) = TestControl::remove_endpoint(0x1234, 0x0001);
        assert!(matches!(msg, ControlMessage::RemoveEndpoint(..)));

        let message = Message::<TestPayload>::new_sd(1, &empty_sd_header());
        let (_send_rx, _resp_rx, msg) = TestControl::send_to_service(0x1234, 0x0001, message);
        assert!(matches!(msg, ControlMessage::SendToService { .. }));

        let (_rx, msg) = TestControl::subscribe(0x1234, 0x0001, 1, 3, 0x01, 0);
        assert!(matches!(msg, ControlMessage::Subscribe { .. }));
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
        let (_rx, msg) = TestControl::add_endpoint(0x1234, 0x0001, addr);
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

    #[tokio::test]
    async fn test_inner_spawn_and_shutdown() {
        let (control_sender, mut update_receiver) = Inner::<TestPayload>::spawn(
            Ipv4Addr::LOCALHOST,
            Arc::new(Mutex::new(E2ERegistry::new())),
        );
        // Drop control sender to trigger loop exit
        drop(control_sender);
        // The update receiver should eventually return None when the inner loop exits
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(2), update_receiver.recv()).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    /// Helper: verify inner loop is still alive by sending an `AddEndpoint` and
    /// checking that a response arrives within 2 seconds.
    async fn assert_inner_alive(control_sender: &Sender<ControlMessage<TestPayload>>) {
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9999);
        let (rx, msg) = TestControl::add_endpoint(0xFFFE, 0xFFFE, addr);
        control_sender.send(msg).await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
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
        let (control_sender, _update_receiver) = Inner::<TestPayload>::spawn(
            Ipv4Addr::LOCALHOST,
            Arc::new(Mutex::new(E2ERegistry::new())),
        );

        let (rx, msg) = TestControl::bind_discovery();
        drop(rx);
        control_sender.send(msg).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        assert_inner_alive(&control_sender).await;
    }

    #[tokio::test]
    async fn test_dropped_receiver_unbind_discovery_continues() {
        let (control_sender, _update_receiver) = Inner::<TestPayload>::spawn(
            Ipv4Addr::LOCALHOST,
            Arc::new(Mutex::new(E2ERegistry::new())),
        );

        let (rx, msg) = TestControl::unbind_discovery();
        drop(rx);
        control_sender.send(msg).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        assert_inner_alive(&control_sender).await;
    }

    #[tokio::test]
    async fn test_dropped_receiver_set_interface_continues() {
        let (control_sender, _update_receiver) = Inner::<TestPayload>::spawn(
            Ipv4Addr::LOCALHOST,
            Arc::new(Mutex::new(E2ERegistry::new())),
        );

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
        let (control_sender, _update_receiver) = Inner::<TestPayload>::spawn(
            Ipv4Addr::LOCALHOST,
            Arc::new(Mutex::new(E2ERegistry::new())),
        );

        // Bind discovery first so the SendSD path has a socket to use
        let (rx, msg) = TestControl::bind_discovery();
        control_sender.send(msg).await.unwrap();
        rx.await.unwrap().unwrap();

        // Send SD with a dropped receiver
        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 30490);
        let sd_header = empty_sd_header();
        let (rx, msg) = TestControl::send_sd(target, sd_header);
        drop(rx);
        control_sender.send(msg).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        assert_inner_alive(&control_sender).await;
    }

    // -- Non-preemptible active_request test --
    // Verifies that when a new control message arrives while a multi-step
    // operation (SetInterface) is mid-way through processing, the new message
    // is rejected and the in-progress operation completes successfully.

    #[tokio::test]
    async fn test_non_preemptible_active_request_rejects_new_message() {
        let (control_sender, _update_receiver) = Inner::<TestPayload>::spawn(
            Ipv4Addr::LOCALHOST,
            Arc::new(Mutex::new(E2ERegistry::new())),
        );

        // Bind discovery so SetInterface will take the multi-step path:
        // iteration 1: unbind discovery, re-queue SetInterface
        // iteration 2: interface matches, bind discovery, send response
        let (rx, msg) = TestControl::bind_discovery();
        control_sender.send(msg).await.unwrap();
        rx.await.unwrap().unwrap();

        // Queue both messages into the channel buffer before the inner loop
        // processes either. mpsc sends on a non-full buffer complete without
        // yielding, so both land before the spawned task runs.
        //
        // 1) SetInterface(LOCALHOST) — will unbind discovery, re-queue itself
        // 2) AddEndpoint — should be rejected while SetInterface is in-flight
        let (rx_set, msg_set) = TestControl::set_interface(Ipv4Addr::LOCALHOST);
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9999);
        let (rx_add, msg_add) = TestControl::add_endpoint(0x1234, 0x0001, addr);
        control_sender.send(msg_set).await.unwrap();
        control_sender.send(msg_add).await.unwrap();

        // SetInterface should complete successfully despite the intervening message
        let set_result = tokio::time::timeout(std::time::Duration::from_secs(3), rx_set)
            .await
            .expect("Timed out waiting for SetInterface")
            .expect("SetInterface oneshot closed");
        assert!(set_result.is_ok());

        // AddEndpoint's oneshot sender was dropped when the non-preemptible
        // arm rejected the message, so the receiver gets RecvError.
        let add_result = tokio::time::timeout(std::time::Duration::from_secs(1), rx_add)
            .await
            .expect("Timed out waiting for AddEndpoint rejection");
        assert!(
            add_result.is_err(),
            "AddEndpoint should have been rejected (oneshot sender dropped)"
        );

        // Verify inner loop is still alive
        assert_inner_alive(&control_sender).await;
    }

    #[test]
    fn test_send_to_service_constructor_returns_two_receivers() {
        let message = Message::<TestPayload>::new_sd(1, &empty_sd_header());
        let (send_rx, resp_rx, _msg) = TestControl::send_to_service(0x1234, 0x0001, message);

        // Extract the senders from the control message
        if let ControlMessage::SendToService {
            send_complete,
            response,
            ..
        } = _msg
        {
            // Both channels are independent — sending on one doesn't affect the other
            send_complete.send(Ok(())).unwrap();
            assert!(send_rx.blocking_recv().unwrap().is_ok());

            let payload = TestPayload {
                header: empty_sd_header(),
            };
            response.send(Ok(payload.clone())).unwrap();
            assert_eq!(resp_rx.blocking_recv().unwrap().unwrap(), payload);
        } else {
            panic!("expected SendToService variant");
        }
    }

    #[tokio::test]
    async fn test_dropped_receiver_add_endpoint_continues() {
        let (control_sender, _update_receiver) = Inner::<TestPayload>::spawn(Ipv4Addr::LOCALHOST);

        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5000);
        let (rx, msg) = TestControl::add_endpoint(0x1234, 0x0001, addr);
        drop(rx);
        control_sender.send(msg).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        assert_inner_alive(&control_sender).await;
    }

    #[tokio::test]
    async fn test_dropped_receiver_remove_endpoint_continues() {
        let (control_sender, _update_receiver) = Inner::<TestPayload>::spawn(Ipv4Addr::LOCALHOST);

        let (rx, msg) = TestControl::remove_endpoint(0x1234, 0x0001);
        drop(rx);
        control_sender.send(msg).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        assert_inner_alive(&control_sender).await;
    }

    #[tokio::test]
    async fn test_dropped_receiver_send_to_service_send_complete_continues() {
        let (control_sender, _update_receiver) = Inner::<TestPayload>::spawn(Ipv4Addr::LOCALHOST);

        // Add an endpoint first so SendToService doesn't fail with ServiceNotFound
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5000);
        let (rx, msg) = TestControl::add_endpoint(0x1234, 0x0001, addr);
        control_sender.send(msg).await.unwrap();
        rx.await.unwrap().unwrap();

        // Send SendToService with the send_complete receiver dropped
        let message = Message::<TestPayload>::new_sd(1, &empty_sd_header());
        let (send_rx, _resp_rx, msg) = TestControl::send_to_service(0x1234, 0x0001, message);
        drop(send_rx);
        control_sender.send(msg).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        assert_inner_alive(&control_sender).await;
    }

    #[tokio::test]
    async fn test_bind_discovery_idempotent() {
        // Binding discovery twice should succeed (early return on already-bound)
        let (control_sender, _update_receiver) = Inner::<TestPayload>::spawn(Ipv4Addr::LOCALHOST);

        let (rx, msg) = TestControl::bind_discovery();
        control_sender.send(msg).await.unwrap();
        rx.await.unwrap().unwrap();

        // Second bind should also succeed (idempotent path)
        let (rx, msg) = TestControl::bind_discovery();
        control_sender.send(msg).await.unwrap();
        rx.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_send_sd_auto_binds_discovery() {
        // SendSD without a bound discovery socket should auto-bind and succeed
        let (control_sender, _update_receiver) = Inner::<TestPayload>::spawn(Ipv4Addr::LOCALHOST);

        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 30490);
        let sd_header = empty_sd_header();
        let (rx, msg) = TestControl::send_sd(target, sd_header);
        control_sender.send(msg).await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
            .await
            .expect("Timed out waiting for SendSD")
            .expect("SendSD oneshot closed");
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_to_service_auto_binds_unicast() {
        // SendToService with no unicast sockets should auto-bind ephemeral
        let (control_sender, _update_receiver) = Inner::<TestPayload>::spawn(Ipv4Addr::LOCALHOST);

        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5000);
        let (rx, msg) = TestControl::add_endpoint(0x1234, 0x0001, addr);
        control_sender.send(msg).await.unwrap();
        rx.await.unwrap().unwrap();

        let message = Message::<TestPayload>::new_sd(1, &empty_sd_header());
        let (send_rx, _resp_rx, msg) = TestControl::send_to_service(0x1234, 0x0001, message);
        control_sender.send(msg).await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), send_rx)
            .await
            .expect("Timed out waiting for SendToService")
            .expect("SendToService oneshot closed");
        assert!(result.is_ok(), "send should succeed: {result:?}");
    }

    #[tokio::test]
    async fn test_subscribe_with_endpoint_sends_sd() {
        // Subscribe with a known endpoint and bound discovery should send the SD message
        let (control_sender, _update_receiver) = Inner::<TestPayload>::spawn(Ipv4Addr::LOCALHOST);

        // Bind discovery first
        let (rx, msg) = TestControl::bind_discovery();
        control_sender.send(msg).await.unwrap();
        rx.await.unwrap().unwrap();

        // Add endpoint
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5000);
        let (rx, msg) = TestControl::add_endpoint(0x1234, 0x0001, addr);
        control_sender.send(msg).await.unwrap();
        rx.await.unwrap().unwrap();

        // Subscribe
        let (rx, msg) = TestControl::subscribe(0x1234, 0x0001, 1, 3, 0x01, 0);
        control_sender.send(msg).await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
            .await
            .expect("Timed out waiting for Subscribe")
            .expect("Subscribe oneshot closed");
        assert!(result.is_ok(), "subscribe should succeed: {result:?}");
    }

    #[tokio::test]
    async fn test_subscribe_auto_binds_discovery() {
        // Subscribe without discovery bound should auto-bind and succeed
        let (control_sender, _update_receiver) = Inner::<TestPayload>::spawn(Ipv4Addr::LOCALHOST);

        // Add endpoint but do NOT bind discovery
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5000);
        let (rx, msg) = TestControl::add_endpoint(0x1234, 0x0001, addr);
        control_sender.send(msg).await.unwrap();
        rx.await.unwrap().unwrap();

        // Subscribe should auto-bind discovery
        let (rx, msg) = TestControl::subscribe(0x1234, 0x0001, 1, 3, 0x01, 0);
        control_sender.send(msg).await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
            .await
            .expect("Timed out waiting for Subscribe")
            .expect("Subscribe oneshot closed");
        assert!(result.is_ok(), "subscribe should auto-bind: {result:?}");
    }

    #[tokio::test]
    async fn test_subscribe_unknown_service_returns_error() {
        let (control_sender, _update_receiver) = Inner::<TestPayload>::spawn(Ipv4Addr::LOCALHOST);

        let (rx, msg) = TestControl::subscribe(0xFFFF, 0xFFFF, 1, 3, 0x01, 0);
        control_sender.send(msg).await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
            .await
            .expect("Timed out")
            .expect("oneshot closed");
        assert!(matches!(result, Err(Error::ServiceNotFound)));
    }

    #[tokio::test]
    async fn test_send_to_service_reuses_existing_unicast_socket() {
        // When a unicast socket already exists, SendToService should reuse it
        let (control_sender, _update_receiver) = Inner::<TestPayload>::spawn(Ipv4Addr::LOCALHOST);

        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5000);
        let (rx, msg) = TestControl::add_endpoint(0x1234, 0x0001, addr);
        control_sender.send(msg).await.unwrap();
        rx.await.unwrap().unwrap();

        // First send auto-binds unicast
        let message = Message::<TestPayload>::new_sd(1, &empty_sd_header());
        let (send_rx, _resp_rx, msg) = TestControl::send_to_service(0x1234, 0x0001, message);
        control_sender.send(msg).await.unwrap();
        send_rx.await.unwrap().unwrap();

        // Second send reuses the existing socket (no auto-bind needed)
        let message = Message::<TestPayload>::new_sd(1, &empty_sd_header());
        let (send_rx, _resp_rx, msg) = TestControl::send_to_service(0x1234, 0x0001, message);
        control_sender.send(msg).await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), send_rx)
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
        let (control_sender, _update_receiver) = Inner::<TestPayload>::spawn(Ipv4Addr::LOCALHOST);

        let (rx, msg) = TestControl::subscribe(0x1234, 0x0001, 1, 3, 0x01, 0);
        drop(rx);
        control_sender.send(msg).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        assert_inner_alive(&control_sender).await;
    }
}
