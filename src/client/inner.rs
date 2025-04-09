use std::{
    future,
    net::{Ipv4Addr, SocketAddrV4},
};
use tokio::{
    select,
    sync::{
        mpsc::{self, Receiver, Sender},
        oneshot,
    },
};
use tracing::{debug, field::debug, info, trace, warn};

use crate::{
    Error,
    client::ClientUpdate,
    client::socket_manager::SocketManager,
    protocol::{Message, sd},
    traits::PayloadWireFormat,
};

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Control<PayloadDefinition> {
    SetInterface(Ipv4Addr),
    BindDiscovery,
    UnbindDiscovery,
    BindUnicast,
    UnbindUnicast,
    SendSD(SocketAddrV4, sd::Header),
    Send(SocketAddrV4, Message<PayloadDefinition>),
}

#[derive(Debug)]
pub(super) struct ControlMessage<PayloadDefinition> {
    control: Control<PayloadDefinition>,
    response: oneshot::Sender<Result<ControlResponse, Error>>,
}

impl<PayloadDefinition> ControlMessage<PayloadDefinition>
where
    PayloadDefinition: PayloadWireFormat,
{
    pub fn new(
        control: Control<PayloadDefinition>,
    ) -> (Self, oneshot::Receiver<Result<ControlResponse, Error>>) {
        let (response, receiver) = oneshot::channel();
        (Self { control, response }, receiver)
    }

    pub fn with_response(
        control: Control<PayloadDefinition>,
        response: oneshot::Sender<Result<ControlResponse, Error>>,
    ) -> Self {
        Self { control, response }
    }
}

#[derive(Debug)]
pub enum ControlResponse {
    Success,
    SocketBind(u16),
}

#[derive(Debug)]
pub(super) struct Inner<PayloadDefinitions> {
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
    /// Socket manager for unicast messages if bound
    unicast_socket: Option<SocketManager<PayloadDefinitions>>,
    /// Phantom data to represent the generic message definitions
    phantom: std::marker::PhantomData<PayloadDefinitions>,
}

impl<PayloadDefinitions> Inner<PayloadDefinitions>
where
    PayloadDefinitions: PayloadWireFormat + Clone + std::fmt::Debug + 'static,
{
    pub fn new(
        interface: Ipv4Addr,
    ) -> (
        Sender<ControlMessage<PayloadDefinitions>>,
        Receiver<ClientUpdate<PayloadDefinitions>>,
    ) {
        info!("Initializing SOME/IP Client");
        let (control_sender, control_receiver) = mpsc::channel(16);
        let (update_sender, update_receiver) = mpsc::channel(16);
        let inner = Self {
            control_receiver,
            active_request: None,
            update_sender,
            interface,
            discovery_socket: None,
            unicast_socket: None,
            phantom: std::marker::PhantomData,
        };
        inner.run();
        (control_sender, update_receiver)
    }

    async fn bind_discovery(&mut self) -> Result<ControlResponse, Error> {
        if self.discovery_socket.is_some() {
            Ok(ControlResponse::Success)
        } else {
            let socket = SocketManager::bind_discovery(self.interface).await?;
            self.discovery_socket = Some(socket);
            Ok(ControlResponse::Success)
        }
    }

    // Dropping the receiver kills the loop
    fn unbind_discovery(&mut self) {
        debug("Unbinding Discovery socket.");
        self.discovery_socket = None;
    }

    async fn set_interface(&mut self, interface: &Ipv4Addr) {
        self.interface = interface.clone();
    }

    async fn bind_unicast(&mut self) -> Result<ControlResponse, Error> {
        if let Some(socket) = &self.unicast_socket {
            Ok(ControlResponse::SocketBind(socket.port()))
        } else {
            let unicast_socket = SocketManager::bind(0).await?;
            let port = unicast_socket.port();
            self.unicast_socket = Some(unicast_socket);
            Ok(ControlResponse::SocketBind(port))
        }
    }

    async fn unbind_unicast(&mut self) {
        self.unicast_socket = None;
    }

    async fn receive_discovery(
        socket_manager: &mut Option<SocketManager<PayloadDefinitions>>,
    ) -> Result<sd::Header, Error> {
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

    async fn receive_unicast(
        socket_manager: &mut Option<SocketManager<PayloadDefinitions>>,
    ) -> Result<Message<PayloadDefinitions>, Error> {
        if let Some(receiver) = socket_manager {
            match receiver.receive().await {
                Some(message) => message,
                None => Err(Error::SocketClosedUnexpectedly),
            }
        } else {
            // If we don't have a receiver, we should return a future that never resolves
            future::pending().await
        }
    }

    async fn handle_control_message(&mut self) {
        if let Some(active_request) = self.active_request.take() {
            let ControlMessage { control, response } = active_request;
            match &control {
                Control::SetInterface(interface) => {
                    if self.discovery_socket.is_some() {
                        info!(
                            "Discovery socket currently bound to interface: {}, unbinding.",
                            self.interface
                        );
                        self.unbind_discovery();
                        self.active_request = Some(ControlMessage::with_response(
                            Control::SetInterface(interface.to_owned()),
                            response,
                        ));

                        return;
                    }
                    if self.interface != *interface {
                        self.set_interface(interface).await;
                        self.active_request = Some(ControlMessage::with_response(
                            Control::SetInterface(interface.to_owned()),
                            response,
                        ));
                        return;
                    }
                    info!("Binding to interface: {}", interface);
                    let bind_result = self.bind_discovery().await;
                    match &bind_result {
                        Ok(_) => {
                            info!("Successfully Bound to interface: {}", interface);
                        }
                        Err(e) => {
                            warn!("Failed to bind to interface: {}. Error: {:?}", interface, e);
                        }
                    }
                    if response.send(bind_result).is_err() {
                        // The sender has been dropped, so we should exit
                        return;
                    }
                }
                Control::BindDiscovery => {
                    let result = self.bind_discovery().await;
                    if response.send(result).is_err() {
                        // The sender has been dropped, so we should exit
                        return;
                    }
                }
                Control::UnbindDiscovery => {
                    self.unbind_discovery();
                    if response.send(Ok(ControlResponse::Success)).is_err() {
                        // The sender has been dropped, so we should exit
                        return;
                    }
                }
                Control::BindUnicast => {
                    if response.send(self.bind_unicast().await).is_err() {
                        // The sender has been dropped, so we should exit
                        return;
                    }
                }
                Control::UnbindUnicast => {
                    self.unbind_unicast().await;
                    if response.send(Ok(ControlResponse::Success)).is_err() {
                        // The sender has been dropped, so we should exit
                        return;
                    }
                }
                Control::SendSD(target, header) => {
                    // SD Message, If the discovery socket is not bound, bind it
                    if self.discovery_socket.is_none() {
                        match self.bind_discovery().await {
                            Ok(_) => {
                                self.active_request = Some(ControlMessage::with_response(
                                    Control::SendSD(*target, header.to_owned()),
                                    response,
                                ));
                                return;
                            }
                            Err(e) => {
                                if response.send(Err(e)).is_err() {
                                    // The sender has been dropped, so we should exit
                                }
                                return;
                            }
                        }
                    }
                    let message = Message::<PayloadDefinitions>::new_sd(
                        self.discovery_socket.as_ref().unwrap().session_id() as u32,
                        header,
                    );
                    debug!("Sending {:?} to {}", &message, target);
                    let send_result = self
                        .discovery_socket
                        .as_mut()
                        .unwrap()
                        .send(*target, message)
                        .await;
                    if response.send(send_result).is_err() {
                        return;
                    }
                }
                Control::Send(target, message) => {
                    if self.unicast_socket.is_none() {
                        if response.send(Err(Error::UnicastSocketNotBound)).is_err() {
                            return;
                        }
                    } else {
                        let send_result = self
                            .unicast_socket
                            .as_mut()
                            .unwrap()
                            .send(*target, message.clone())
                            .await;
                        if response.send(send_result).is_err() {
                            // The sender has been dropped, so we should exit
                            return;
                        }
                    }
                }
            }
        }
    }

    fn run(mut self) {
        tokio::spawn(async move {
            debug!("SOME/IP Client processing loop started");
            loop {
                let Self {
                    control_receiver,
                    discovery_socket,
                    unicast_socket,
                    update_sender,
                    ..
                } = &mut self;
                select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {}
                    // Receive a control message
                    ctrl = control_receiver.recv() => {
                        if let Some(ctrl) = ctrl {
                            assert!(self.active_request.is_none());
                            debug!("Received control message: {:?}", ctrl.control);
                            self.active_request = Some(ctrl);
                        } else {
                            // The sender has been dropped, so we should exit
                            break;
                        }
                    }
                    // Receive a discovery message
                    discovery = Inner::receive_discovery(discovery_socket) => {
                        trace!("Received discovery message: {:?}", discovery);
                        match discovery {
                            Ok(header) => {
                                if update_sender.send(ClientUpdate::DiscoveryUpdated(header)).await.is_err() {
                                    // The sender has been dropped, so we should exit
                                    break;
                                }
                            }
                            Err(err) => {
                                if update_sender.send(ClientUpdate::Error(err)).await.is_err() {
                                    // The sender has been dropped, so we should exit
                                    break;
                                }
                            }
                        }
                     }
                     unicast = Inner::receive_unicast(unicast_socket) => {
                         trace!("Received unicast message: {:?}",unicast);
                         match unicast {
                             Ok(message) => {
                                 if update_sender.send(ClientUpdate::Unicast(message)).await.is_err() {
                                     // The sender has been dropped, so we should exit
                                     break;
                                 }
                             }
                             Err(err) => {
                                 if update_sender.send(ClientUpdate::Error(err)).await.is_err() {
                                     // The sender has been dropped, so we should exit
                                     break;
                                 }
                             }
                         }
                     }
                }
                self.handle_control_message().await;
            }
        });
    }
}
