use std::{
    collections::HashMap,
    future,
    net::{Ipv4Addr, SocketAddrV4},
    prelude::rust_2024::*,
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
    Error,
    client::{ClientUpdate, socket_manager::SocketManager},
    protocol::Message,
    traits::PayloadWireFormat,
};

pub(super) enum ControlMessage<P: PayloadWireFormat> {
    SetInterface(Ipv4Addr, oneshot::Sender<Result<(), Error>>),
    BindDiscovery(oneshot::Sender<Result<(), Error>>),
    UnbindDiscovery(oneshot::Sender<Result<(), Error>>),
    BindUnicast(oneshot::Sender<Result<u16, Error>>, Option<u16>),
    UnbindUnicast(oneshot::Sender<Result<(), Error>>),
    SendSD(
        SocketAddrV4,
        P::SdHeader,
        oneshot::Sender<Result<(), Error>>,
    ),
    Send(
        SocketAddrV4,
        Message<P>,
        u16, // source port — which unicast socket to send from
        oneshot::Sender<Result<P, Error>>,
    ),
    AwaitResponse(Message<P>, oneshot::Sender<Result<P, Error>>),
}

impl<P: PayloadWireFormat> std::fmt::Debug for ControlMessage<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SetInterface(addr, _) => f.debug_tuple("SetInterface").field(addr).finish(),
            Self::BindDiscovery(_) => f.write_str("BindDiscovery"),
            Self::UnbindDiscovery(_) => f.write_str("UnbindDiscovery"),
            Self::BindUnicast(_, port) => f.debug_tuple("BindUnicast").field(port).finish(),
            Self::UnbindUnicast(_) => f.write_str("UnbindUnicast"),
            Self::SendSD(addr, header, _) => {
                f.debug_tuple("SendSD").field(addr).field(header).finish()
            }
            Self::Send(addr, msg, port, _) => f
                .debug_tuple("Send")
                .field(addr)
                .field(msg)
                .field(port)
                .finish(),
            Self::AwaitResponse(msg, _) => f.debug_tuple("AwaitResponse").field(msg).finish(),
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

    pub fn bind_unicast_with_port(
        port: Option<u16>,
    ) -> (oneshot::Receiver<Result<u16, Error>>, Self) {
        let (sender, receiver) = oneshot::channel();
        (receiver, Self::BindUnicast(sender, port))
    }
    pub fn unbind_unicast() -> (oneshot::Receiver<Result<(), Error>>, Self) {
        let (sender, receiver) = oneshot::channel();
        (receiver, Self::UnbindUnicast(sender))
    }

    pub fn send_sd(
        socket_addr: SocketAddrV4,
        header: P::SdHeader,
    ) -> (oneshot::Receiver<Result<(), Error>>, Self) {
        let (sender, receiver) = oneshot::channel();
        (receiver, Self::SendSD(socket_addr, header, sender))
    }
    pub fn send_request(
        socket_addr: SocketAddrV4,
        message: Message<P>,
        source_port: u16,
    ) -> (oneshot::Receiver<Result<P, Error>>, Self) {
        let (sender, receiver) = oneshot::channel();
        (
            receiver,
            Self::Send(socket_addr, message, source_port, sender),
        )
    }
}

pub(super) struct Inner<PayloadDefinitions: PayloadWireFormat> {
    /// MPSC Receiver used to receive control messages from outer client
    control_receiver: Receiver<ControlMessage<PayloadDefinitions>>,
    /// The active request, if one is being served
    active_request: Option<ControlMessage<PayloadDefinitions>>,
    /// MPSC Sender used to send updates to outer client
    update_sender: mpsc::Sender<ClientUpdate<PayloadDefinitions>>,
    /// Target interface for sockets
    interface: Ipv4Addr,
    /// Socket manager for service discovery if bound
    discovery_socket: Option<SocketManager<PayloadDefinitions>>,
    /// Socket managers for unicast messages, keyed by local port
    unicast_sockets: HashMap<u16, SocketManager<PayloadDefinitions>>,
    /// Internal flag to continue run loop
    run: bool,
    /// Client ID for SOME/IP request headers (upper 16 bits of request ID)
    client_id: u16,
    /// Incrementing session counter for SOME/IP request headers (lower 16 bits of request ID)
    session_counter: u16,
    /// Phantom data to represent the generic message definitions
    phantom: std::marker::PhantomData<PayloadDefinitions>,
}

impl<PayloadDefinitions: PayloadWireFormat> std::fmt::Debug for Inner<PayloadDefinitions> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("interface", &self.interface)
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
            update_sender,
            interface,
            discovery_socket: None,
            unicast_sockets: HashMap::new(),
            run: true,
            client_id: 0x1234,
            session_counter: 1,
            phantom: std::marker::PhantomData,
        };
        inner.run();
        (control_sender, update_receiver)
    }

    fn bind_discovery(&mut self) -> Result<(), Error> {
        if self.discovery_socket.is_some() {
            Ok(())
        } else {
            let socket = SocketManager::bind_discovery(self.interface)?;
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
        let unicast_socket = SocketManager::bind(port)?;
        let bound_port = unicast_socket.port();
        self.unicast_sockets.insert(bound_port, unicast_socket);
        debug!("Bound unicast socket on port {}", bound_port);
        Ok(bound_port)
    }

    fn unbind_unicast(&mut self) {
        self.unicast_sockets.clear();
    }

    async fn receive_discovery(
        socket_manager: &mut Option<SocketManager<PayloadDefinitions>>,
    ) -> Result<<PayloadDefinitions as PayloadWireFormat>::SdHeader, Error> {
        if let Some(receiver) = socket_manager {
            match receiver.receive().await {
                Some(message) => match message {
                    Ok(message) => {
                        if let Some(header) = message.get_sd_header() {
                            Ok(header.to_owned())
                        } else {
                            Err(Error::UnexpectedDiscoveryMessage(
                                message.header().to_owned(),
                            ))
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
    ) -> Result<Message<PayloadDefinitions>, Error> {
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
                        // The sender has been dropped, so we should exit
                        self.run = false;
                    }
                }
                ControlMessage::BindDiscovery(response) => {
                    let result = self.bind_discovery();
                    if response.send(result).is_err() {
                        // The sender has been dropped, so we should exit
                        self.run = false;
                    }
                }
                ControlMessage::UnbindDiscovery(response) => {
                    self.unbind_discovery().await;
                    if response.send(Ok(())).is_err() {
                        // The sender has been dropped, so we should exit
                        self.run = false;
                    }
                }
                ControlMessage::BindUnicast(response, port) => {
                    let result = self.bind_unicast(port.unwrap_or(0));
                    match response.send(result) {
                        Ok(()) => (),
                        Err(e) => {
                            error!("Failed to send bind unicast response: {:?}", e);
                            self.run = false;
                        }
                    }
                }
                ControlMessage::UnbindUnicast(response) => {
                    self.unbind_unicast();
                    if response.send(Ok(())).is_err() {
                        // The sender has been dropped, so we should exit
                        self.run = false;
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
                                        self.run = false;
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
                                self.run = false;
                            }
                        }
                    }
                }
                ControlMessage::Send(target, mut message, source_port, response) => {
                    if let Some(socket) = self.unicast_sockets.get_mut(&source_port) {
                        // Set client ID (upper 16) and session ID (lower 16)
                        let request_id =
                            (u32::from(self.client_id) << 16) | u32::from(self.session_counter);
                        message.set_request_id(request_id);
                        self.session_counter = self.session_counter.wrapping_add(1);
                        if self.session_counter == 0 {
                            self.session_counter = 1;
                        }
                        let send_result = socket.send(target, message.clone()).await;
                        match send_result {
                            Ok(()) => {
                                self.active_request =
                                    Some(ControlMessage::AwaitResponse(message.clone(), response));
                            }
                            Err(_) => todo!(),
                        }
                    } else {
                        error!("No unicast socket bound on port {}", source_port);
                        let _ = response.send(Err(Error::UnicastSocketNotBound));
                    }
                }
                // Nothing to do here, this is handled in the run loop when receiving messages
                ControlMessage::AwaitResponse(message, response) => {
                    self.active_request = Some(ControlMessage::AwaitResponse(message, response));
                }
            }
        }
    }

    fn run(mut self) {
        tokio::spawn(async move {
            info!("SOME/IP Client processing loop started");
            loop {
                let Self {
                    control_receiver,
                    discovery_socket,
                    unicast_sockets,
                    update_sender,
                    active_request,
                    run,
                    ..
                } = &mut self;
                select! {
                    () = tokio::time::sleep(std::time::Duration::from_millis(125)) => {}
                    // Receive a control message
                    ctrl = control_receiver.recv() => {
                        if let Some(ctrl) = ctrl {
                            assert!(active_request.is_none());
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
                            Ok(header) => {
                                if update_sender.send(ClientUpdate::DiscoveryUpdated(header)).await.is_err() {
                                    // The sender has been dropped, so we should exit
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
                             Ok(received_message) => {
                                 if let Some(active) = active_request.take() {
                                     if let ControlMessage::AwaitResponse(request_message, response) = active {
                                         if request_message.header().message_id == received_message.header().message_id {
                                            if response.send(Ok(
                                                 received_message.payload().clone(),
                                             )).is_err() {
                                                 // The sender has been dropped, so we should exit
                                                 *run = false;
                                             }
                                             else {
                                                 *active_request = None;
                                             }
                                         } else {
                                             *active_request = Some(ControlMessage::AwaitResponse(request_message, response));
                                             // Use try_send to avoid blocking the select loop
                                             // while waiting for a response. If the channel is
                                             // full, drop the event rather than deadlocking.
                                             match update_sender.try_send(ClientUpdate::Unicast(received_message)) {
                                                 Ok(()) => {}
                                                 Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                                     trace!("Update channel full, dropping event while awaiting response");
                                                 }
                                                 Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                                     *run = false;
                                                 }
                                             }
                                         }
                                     } else {*active_request = Some(active);}
                                 } else if update_sender.send(ClientUpdate::Unicast(received_message)).await.is_err(){
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
    use crate::traits::DiscoveryOnlyPayload;
    use std::format;

    type TestControl = ControlMessage<DiscoveryOnlyPayload>;

    #[test]
    fn test_control_message_constructors() {
        // Each constructor returns (oneshot::Receiver, ControlMessage)
        let (_rx, msg) = TestControl::set_interface(Ipv4Addr::LOCALHOST);
        assert!(matches!(msg, ControlMessage::SetInterface(..)));

        let (_rx, msg) = TestControl::bind_discovery();
        assert!(matches!(msg, ControlMessage::BindDiscovery(..)));

        let (_rx, msg) = TestControl::unbind_discovery();
        assert!(matches!(msg, ControlMessage::UnbindDiscovery(..)));

        let (_rx, msg) = TestControl::bind_unicast_with_port(Some(0));
        assert!(matches!(msg, ControlMessage::BindUnicast(..)));

        let (_rx, msg) = TestControl::unbind_unicast();
        assert!(matches!(msg, ControlMessage::UnbindUnicast(..)));

        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1234);
        let sd_header = crate::protocol::sd::Header::new_find_services(false, &[]);
        let (_rx, msg) = TestControl::send_sd(target, sd_header);
        assert!(matches!(msg, ControlMessage::SendSD(..)));

        let message = Message::new_sd(
            1,
            &crate::protocol::sd::Header::new_find_services(false, &[]),
        );
        let (_rx, msg) = TestControl::send_request(target, message, 5000);
        assert!(matches!(msg, ControlMessage::Send(..)));
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

        let (_rx, msg) = TestControl::bind_unicast_with_port(Some(8080));
        assert!(format!("{msg:?}").contains("BindUnicast"));

        let (_rx, msg) = TestControl::unbind_unicast();
        assert!(format!("{msg:?}").contains("UnbindUnicast"));

        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1234);
        let sd_header = crate::protocol::sd::Header::new_find_services(false, &[]);
        let (_rx, msg) = TestControl::send_sd(target, sd_header);
        assert!(format!("{msg:?}").contains("SendSD"));

        let message = Message::new_sd(
            1,
            &crate::protocol::sd::Header::new_find_services(false, &[]),
        );
        let (_rx, msg) = TestControl::send_request(target, message, 5000);
        assert!(format!("{msg:?}").contains("Send"));
    }

    #[tokio::test]
    async fn test_inner_spawn_and_shutdown() {
        let (control_sender, mut update_receiver) =
            Inner::<DiscoveryOnlyPayload>::spawn(Ipv4Addr::LOCALHOST);
        // Drop control sender to trigger loop exit
        drop(control_sender);
        // The update receiver should eventually return None when the inner loop exits
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(2), update_receiver.recv()).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_inner_bind_unicast_via_channel() {
        let (control_sender, _update_receiver) =
            Inner::<DiscoveryOnlyPayload>::spawn(Ipv4Addr::LOCALHOST);

        let (rx, msg) = TestControl::bind_unicast_with_port(None);
        control_sender.send(msg).await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
            .await
            .expect("Timed out waiting for bind response")
            .expect("Oneshot channel closed");
        let port = result.unwrap();
        assert!(port > 0);
    }
}
