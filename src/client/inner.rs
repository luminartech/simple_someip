use std::{
    future,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    thread::sleep,
    time::Duration,
};

use tokio::{
    net::UdpSocket,
    select,
    sync::{
        mpsc::{self, Receiver, Sender},
        oneshot,
    },
};
use tracing::{debug, field::debug, info, trace};

use crate::{
    Error, SD_MULTICAST_IP, SD_MULTICAST_PORT,
    protocol::{self, Message, sd},
    traits::{PayloadWireFormat, WireFormat},
};

use super::{ClientUpdate, DiscoveryInfo};

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Control<PayloadDefinition> {
    SetInterface(Ipv4Addr),
    BindDiscovery,
    UnbindDiscovery,
    BindUnicast,
    UnbindUnicast,
    SendSD(sd::Header),
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
    /// MPSC Receiver used to receive discovery messages
    discovery_receiver: Option<mpsc::Receiver<Result<Message<PayloadDefinitions>, Error>>>,
    /// Discovery information containing endpoint information
    discovery_info: DiscoveryInfo,
    /// Unicast information containing the receiver and port if a unicase socket is bound
    unicast_info: Option<UnicastInfo<PayloadDefinitions>>,
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
            discovery_receiver: None,
            discovery_info: DiscoveryInfo::new(),
            unicast_info: None,
            phantom: std::marker::PhantomData,
        };
        inner.run();
        (control_sender, update_receiver)
    }

    async fn bind_discovery(&mut self) -> Result<(), Error> {
        let (sender, receiver) = mpsc::channel(16);
        let bind_addr =
            std::net::SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), SD_MULTICAST_PORT);
        let mut discovery_socket = UdpSocket::bind(bind_addr).await?;
        info!(
            "Bound Discovery socket to: {}",
            discovery_socket.local_addr().unwrap()
        );
        discovery_socket.join_multicast_v4(SD_MULTICAST_IP, self.interface)?;
        tokio::spawn(async move {
            let mut buf = vec![0; 1400];
            loop {
                select! {
                    message = receive::<PayloadDefinitions>(&mut discovery_socket, &mut buf) => {
                        match sender.send(message).await {
                            Ok(_) => {}
                            Err(_) => {
                                info!("Discovery Socket Dropping");
                                // The receiver has been dropped, so we should exit
                                break;
                            }
                        }
                    },
                    _ = tokio::time::sleep(std::time::Duration::from_millis(125)) => {}
                }
                if sender.is_closed() {
                    break;
                }
            }
        });
        self.discovery_receiver = Some(receiver);
        Ok(())
    }

    // Dropping the receiver kills the loop
    fn unbind_discovery(&mut self) {
        debug("Unbinding Discovery socket.");
        self.discovery_receiver = None;
    }

    async fn set_interface(&mut self, interface: Ipv4Addr) {
        self.interface = interface;
    }

    async fn bind_unicast(&mut self, interface: Ipv4Addr) -> Result<(), Error> {
        let (sender, receiver) = mpsc::channel(16);
        let bind_addr = SocketAddr::new(IpAddr::V4(interface), 0);
        let unicast_socket = UdpSocket::bind(bind_addr).await?;
        // We've bound the socket successfully, so we can store the unicast info
        self.unicast_info = Some(UnicastInfo {
            receiver,
            port: unicast_socket.local_addr().unwrap().port(),
        });
        tokio::spawn(async move {
            let mut buf = vec![0; 1400];
            loop {
                select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {
                        if sender.send(None).await.is_err() {
                            // The receiver has been dropped, so we should exit
                            break;
                        }
                    }
                    Ok((_, _)) = unicast_socket.recv_from(&mut buf) => {
                        println!("Unicast message received");
                        match Message::<PayloadDefinitions>::from_reader(&mut buf.as_slice()) {
                            Ok(message) => {
                                println!("Received unicast message: {:?}", message);
                                if sender.send(Some(message)).await.is_err() {
                                    break;
                                }
                            }
                            Err(_) => {
                                if sender.send(None).await.is_err() {
                                    // The receiver has been dropped, so we should exit
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        });
        Ok(())
    }

    async fn unbind_unicast(&mut self) {
        self.unicast_info = None;
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }

    async fn receive_discovery(
        discovery_receiver: &mut Option<mpsc::Receiver<Result<Message<PayloadDefinitions>, Error>>>,
    ) -> Result<sd::Header, Error> {
        if let Some(receiver) = discovery_receiver {
            match receiver.recv().await {
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

    /*
        async fn receive_unicast(&mut self) -> Result<MessageDefinitions, Error> {
            if let Some(unicast_info) = self.unicast_info.as_ref() {
                unicast_info.receiver.recv().await
            } else {
                None
            }
        }
    */

    async fn handle_control_message(&mut self) {
        if let Some(active_request) = self.active_request.take() {
            match active_request.control {
                Control::SetInterface(interface) => {
                    info!("Binding to interface: {}", interface);
                    if self.discovery_receiver.is_some() {
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
                        .send(self.bind_unicast(self.interface).await)
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
                Control::SendSD(header) => todo!(),
                Control::Send(message) => todo!(),
            }
        }
    }

    fn run(mut self) {
        tokio::spawn(async move {
            debug!("SOME/IP Client processing loop started");
            loop {
                let Self {
                    control_receiver,
                    discovery_receiver,
                    discovery_info,
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
                    discovery = Inner::receive_discovery(discovery_receiver) => {
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

async fn receive<MessageDefinitions: PayloadWireFormat>(
    socket: &mut UdpSocket,
    buf: &mut Vec<u8>,
) -> Result<Message<MessageDefinitions>, Error> {
    match socket.recv_from(buf).await {
        Ok((_received, _origin)) => {
            match Message::<MessageDefinitions>::from_reader(&mut buf.as_slice()) {
                Ok(message) => {
                    trace!("Received message: {:?}", message);
                    Ok(message)
                }
                Err(err) => Err(Error::from(err)),
            }
        }
        Err(err) => Err(Error::from(err)),
    }
}
