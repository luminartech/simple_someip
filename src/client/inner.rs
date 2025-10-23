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
use tracing::{debug, error, info, trace, warn};

use crate::{
    Error,
    client::{ClientUpdate, socket_manager::SocketManager},
    protocol::{Message, sd},
    traits::PayloadWireFormat,
};

#[derive(Debug)]
pub(super) enum ControlMessage<MessageDefinitions> {
    SetInterface(Ipv4Addr, oneshot::Sender<Result<(), Error>>),
    BindDiscovery(oneshot::Sender<Result<(), Error>>),
    UnbindDiscovery(oneshot::Sender<Result<(), Error>>),
    BindUnicast(oneshot::Sender<Result<u16, Error>>),
    UnbindUnicast(oneshot::Sender<Result<(), Error>>),
    SendSD(SocketAddrV4, sd::Header, oneshot::Sender<Result<(), Error>>),
    Send(
        SocketAddrV4,
        Message<MessageDefinitions>,
        oneshot::Sender<Result<MessageDefinitions, Error>>,
    ),
    AwaitResponse(
        Message<MessageDefinitions>,
        oneshot::Sender<Result<MessageDefinitions, Error>>,
    ),
}

impl<MessageDefinitions> ControlMessage<MessageDefinitions> {
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
    pub fn bind_unicast() -> (oneshot::Receiver<Result<u16, Error>>, Self) {
        let (sender, receiver) = oneshot::channel();
        (receiver, Self::BindUnicast(sender))
    }
    pub fn unbind_unicast() -> (oneshot::Receiver<Result<(), Error>>, Self) {
        let (sender, receiver) = oneshot::channel();
        (receiver, Self::UnbindUnicast(sender))
    }

    pub fn send_sd(
        socket_addr: SocketAddrV4,
        header: sd::Header,
    ) -> (oneshot::Receiver<Result<(), Error>>, Self) {
        let (sender, receiver) = oneshot::channel();
        (receiver, Self::SendSD(socket_addr, header, sender))
    }
    pub fn send_request(
        socket_addr: SocketAddrV4,
        message: Message<MessageDefinitions>,
    ) -> (oneshot::Receiver<Result<MessageDefinitions, Error>>, Self) {
        let (sender, receiver) = oneshot::channel();
        (receiver, Self::Send(socket_addr, message, sender))
    }
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
    /// Internal flag to continue run loop
    run: bool,
    /// Phantom data to represent the generic message definitions
    phantom: std::marker::PhantomData<PayloadDefinitions>,
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
            unicast_socket: None,
            run: true,
            phantom: std::marker::PhantomData,
        };
        inner.run();
        (control_sender, update_receiver)
    }

    async fn bind_discovery(&mut self) -> Result<(), Error> {
        if self.discovery_socket.is_some() {
            Ok(())
        } else {
            let socket = SocketManager::bind_discovery(self.interface).await?;
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

    async fn set_interface(&mut self, interface: &Ipv4Addr) {
        self.interface = *interface;
    }

    async fn bind_unicast(&mut self) -> Result<u16, Error> {
        if let Some(socket) = &self.unicast_socket {
            Ok(socket.port())
        } else {
            let unicast_socket = SocketManager::bind(0).await?;
            let port = unicast_socket.port();
            self.unicast_socket = Some(unicast_socket);
            Ok(port)
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
                        self.set_interface(&interface).await;
                        self.active_request =
                            Some(ControlMessage::SetInterface(interface, response));
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
                        self.run = false;
                    }
                }
                ControlMessage::BindDiscovery(response) => {
                    let result = self.bind_discovery().await;
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
                ControlMessage::BindUnicast(response) => {
                    let result = self.bind_unicast().await;
                    match response.send(result) {
                        Ok(_) => (),
                        Err(e) => {
                            error!("Failed to send bind unicast response: {:?}", e);
                            self.run = false;
                        }
                    }
                }
                ControlMessage::UnbindUnicast(response) => {
                    self.unbind_unicast().await;
                    if response.send(Ok(())).is_err() {
                        // The sender has been dropped, so we should exit
                        self.run = false;
                    }
                }
                ControlMessage::SendSD(target, header, response) => {
                    // SD Message, If the discovery socket is not bound, bind it
                    match &mut self.discovery_socket {
                        None => {
                            match self.bind_discovery().await {
                                Ok(_) => {
                                    // Discovery socket successfully bound, send the message on the next loop
                                    self.active_request =
                                        Some(ControlMessage::SendSD(target, header, response));
                                    return;
                                }
                                Err(e) => {
                                    error!(
                                        "Failed to bind discovery socket for sending SD message: {:?}",
                                        e
                                    );
                                    if response.send(Err(e)).is_err() {
                                        self.run = false;
                                    }
                                    return;
                                }
                            }
                        }
                        Some(discovery_socket) => {
                            let message = Message::<PayloadDefinitions>::new_sd(
                                discovery_socket.session_id() as u32,
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
                ControlMessage::Send(target, message, response) => {
                    if let Some(socket) = &mut self.unicast_socket {
                        let send_result = socket.send(target, message.clone()).await;
                        match send_result {
                            Ok(_) => {
                                self.active_request = Some(ControlMessage::AwaitResponse(
                                    message.to_owned(),
                                    response,
                                ))
                            }
                            Err(_) => todo!(),
                        };
                    }
                }
                // Nothing to do here, this is handled in the run loop when receiving messages
                ControlMessage::AwaitResponse(message, response) => {
                    self.active_request = Some(ControlMessage::AwaitResponse(message, response))
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
                    unicast_socket,
                    update_sender,
                    active_request,
                    run,
                    ..
                } = &mut self;
                select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(125)) => {}
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
                     unicast = Inner::receive_unicast(unicast_socket) => {
                         trace!("Received unicast message: {:?}",unicast);
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
                                             if update_sender.send(ClientUpdate::Unicast(received_message)).await.is_err(){
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
