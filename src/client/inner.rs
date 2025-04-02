use std::{future, net::Ipv4Addr, thread::sleep, time::Duration};

use tokio::{
    select,
    sync::{
        mpsc::{self, Receiver, Sender},
        oneshot,
    },
};
use tracing::{debug, field::debug, info, warn};

use crate::{
    DiscoveryInfo, Error, SD_MULTICAST_IP, SD_MULTICAST_PORT,
    client::ClientUpdate,
    protocol::{Message, sd},
    traits::PayloadWireFormat,
};

use super::socket_manager::SocketManager;

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Control<PayloadDefinition> {
    SetInterface(Ipv4Addr),
    BindDiscovery,
    UnbindDiscovery,
    BindUnicast,
    UnbindUnicast,
    Send(Message<PayloadDefinition>),
}

#[derive(Debug)]
pub(super) struct ControlMessage<PayloadDefinition> {
    control: Control<PayloadDefinition>,
    response: oneshot::Sender<Result<(), Error>>,
}

impl<PayloadDefinition> ControlMessage<PayloadDefinition>
where
    PayloadDefinition: PayloadWireFormat,
{
    pub fn new(
        control: Control<PayloadDefinition>,
    ) -> (Self, oneshot::Receiver<Result<(), Error>>) {
        let (response, receiver) = oneshot::channel();
        (Self { control, response }, receiver)
    }
}

#[derive(Debug)]
struct UnicastInfo<PayloadDefinitions> {
    receiver: mpsc::Receiver<Option<Message<PayloadDefinitions>>>,
    session_id: u16,
    port: u16,
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
    /// Discovery information containing endpoint information
    discovery_info: DiscoveryInfo,
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
            discovery_info: DiscoveryInfo::new(),
            discovery_socket: None,
            unicast_socket: None,
            phantom: std::marker::PhantomData,
        };
        inner.run();
        (control_sender, update_receiver)
    }

    async fn bind_discovery(&mut self) -> Result<(), Error> {
        if self.discovery_socket.is_some() {
            warn!("Discovery socket already bound!");
            Ok(())
        } else {
            self.discovery_socket = Some(
                SocketManager::bind_multicast(self.interface, SD_MULTICAST_IP, SD_MULTICAST_PORT)
                    .await?,
            );
            Ok(())
        }
    }

    // Dropping the receiver kills the loop
    fn unbind_discovery(&mut self) {
        debug("Unbinding Discovery socket.");
        self.discovery_socket = None;
    }

    async fn set_interface(&mut self, interface: Ipv4Addr) {
        self.interface = interface;
    }

    async fn bind_unicast(&mut self) -> Result<(), Error> {
        if self.unicast_socket.is_some() {
            warn!("Unicast socket already bound!");
            Ok(())
        } else {
            self.unicast_socket = Some(SocketManager::bind(0).await?);
            Ok(())
        }
    }

    async fn unbind_unicast(&mut self) {
        self.unicast_socket = None;
    }

    fn update_message(&self, message: &mut Message<PayloadDefinitions>) {
        if message.is_sd() {
            let header = message.header_mut();
            let sd_header = message.get_sd_header_mut().unwrap();
        } else {
        }
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
                None => todo!(),
            }
        } else {
            // If we don't have a receiver, we should return a future that never resolves
            future::pending().await
        }
    }

    async fn handle_control_message(&mut self) {
        if let Some(active_request) = self.active_request.take() {
            match active_request.control {
                Control::SetInterface(interface) => {
                    info!("Binding to interface: {}", interface);
                    if self.discovery_socket.is_some() {
                        self.unbind_discovery();
                        sleep(Duration::from_millis(250));
                        self.active_request = Some(active_request);
                        return;
                    }
                    self.set_interface(interface).await;
                    if active_request
                        .response
                        .send(self.bind_discovery().await)
                        .is_err()
                    {
                        // The sender has been dropped, so we should exit
                        return;
                    }
                }
                Control::BindDiscovery => {
                    let result = self.bind_discovery().await;
                    if active_request.response.send(result).is_err() {
                        // The sender has been dropped, so we should exit
                        return;
                    }
                }
                Control::UnbindDiscovery => {
                    self.unbind_discovery();
                    if active_request.response.send(Ok(())).is_err() {
                        // The sender has been dropped, so we should exit
                        return;
                    }
                }
                Control::BindUnicast => {
                    if active_request
                        .response
                        .send(self.bind_unicast().await)
                        .is_err()
                    {
                        // The sender has been dropped, so we should exit
                        return;
                    }
                }
                Control::UnbindUnicast => {
                    self.unbind_unicast().await;
                    if active_request.response.send(Ok(())).is_err() {
                        // The sender has been dropped, so we should exit
                        return;
                    }
                }
                Control::Send(mut message) => {
                    self.update_message(&mut message);
                    if message.is_sd() {}
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
                    discovery_info,
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
                            debug!("Received control message: {:?}", ctrl.control);
                            self.active_request = Some(ctrl);
                        } else {
                            // The sender has been dropped, so we should exit
                            break;
                        }
                    }
                    // Receive a discovery message
                    discovery = Inner::receive_discovery(discovery_socket) => {
                        match discovery {
                            Ok(header) => {
                                match discovery_info.update(header) {
                                    Ok(info) => {
                                        if update_sender.send(ClientUpdate::DiscoveryUpdated(info)).await.is_err() {
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
