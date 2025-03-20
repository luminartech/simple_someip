use std::{
    future,
    net::{IpAddr, Ipv4Addr, SocketAddr},
};

use tokio::{
    net::UdpSocket,
    select,
    sync::{
        mpsc::{self, Receiver, Sender},
        oneshot,
    },
};

use crate::{
    Error, SD_MULTICAST_IP, SD_MULTICAST_PORT,
    protocol::{Message, sd},
    traits::{PayloadWireFormat, WireFormat},
};

use super::{ClientUpdate, DiscoveryInfo};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Control {
    SetInterface(Ipv4Addr),
    BindDiscovery,
    UnbindDiscovery,
    BindUnicast,
    UnbindUnicast,
}

#[derive(Debug)]
pub(super) struct ControlMessage {
    control: Control,
    step: u8,
    response: oneshot::Sender<Result<(), Error>>,
}

impl ControlMessage {
    pub fn new(control: Control) -> (Self, oneshot::Receiver<Result<(), Error>>) {
        let (response, receiver) = oneshot::channel();
        (
            Self {
                control,
                step: 0,
                response,
            },
            receiver,
        )
    }
}

#[derive(Debug)]
struct UnicastInfo<MessageDefinitions> {
    receiver: mpsc::Receiver<Option<Message<MessageDefinitions>>>,
    port: u16,
}

#[derive(Debug)]
pub(super) struct Inner<MessageDefinitions> {
    /// MPSC Receiver used to receive control messages from outer client
    control_receiver: Receiver<ControlMessage>,
    /// The active request, if one is being served
    active_request: Option<ControlMessage>,
    /// MPSC Sender used to send updates to outer client
    update_sender: mpsc::Sender<ClientUpdate<MessageDefinitions>>,
    /// Target interface for sockets
    interface: Ipv4Addr,
    /// MPSC Receiver used to receive discovery messages
    discovery_receiver: Option<mpsc::Receiver<Result<Message<MessageDefinitions>, Error>>>,
    /// Discovery information containing endpoint information
    discovery_info: DiscoveryInfo,
    /// Unicast information containing the receiver and port if a unicase socket is bound
    unicast_info: Option<UnicastInfo<MessageDefinitions>>,
    /// Phantom data to represent the generic message definitions
    phantom: std::marker::PhantomData<MessageDefinitions>,
}

impl<MessageDefinitions> Inner<MessageDefinitions>
where
    MessageDefinitions: PayloadWireFormat + Clone + std::fmt::Debug + 'static,
{
    pub fn new(
        interface: Ipv4Addr,
    ) -> (
        Sender<ControlMessage>,
        Receiver<ClientUpdate<MessageDefinitions>>,
    ) {
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
        discovery_socket.join_multicast_v4(SD_MULTICAST_IP, self.interface)?;
        tokio::spawn(async move {
            let mut buf = vec![0; 1400];
            loop {
                match sender
                    .send(receive::<MessageDefinitions>(&mut discovery_socket, &mut buf).await)
                    .await
                {
                    Ok(_) => {}
                    Err(_) => {
                        // The receiver has been dropped, so we should exit
                        break;
                    }
                }
            }
        });
        self.discovery_receiver = Some(receiver);
        Ok(())
    }

    // Dropping the receiver kills the loop
    fn unbind_discovery(&mut self) {
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
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                        if sender.send(None).await.is_err() {
                            // The receiver has been dropped, so we should exit
                            break;
                        }
                    }
                    Ok((_, _)) = unicast_socket.recv_from(&mut buf) => {
                        println!("Unicast message received");
                        match Message::<MessageDefinitions>::from_reader(&mut buf.as_slice()) {
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
        discovery_receiver: &mut Option<mpsc::Receiver<Result<Message<MessageDefinitions>, Error>>>,
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

    async fn handle_control_message(&mut self, control: ControlMessage) {
        if let Some(ControlMessage {
            control,
            mut step,
            response,
        }) = self.active_request
        {
            match control {
                Control::SetInterface(interface) => {
                    if step == 0 {
                        if response.send(Ok(())).is_err() {
                            // The sender has been dropped, so we should exit
                            return;
                        }
                        step += 1;
                    }
                }
                Control::BindDiscovery => {
                    if step == 0 {
                        if response.send(self.bind_discovery().await).is_err() {
                            // The sender has been dropped, so we should exit
                            return;
                        }
                        step += 1;
                    }
                }
                Control::UnbindDiscovery => {
                    if step == 0 {
                        self.unbind_discovery();
                        if response.send(Ok(())).is_err() {
                            // The sender has been dropped, so we should exit
                            return;
                        }
                    }
                }
                Control::BindUnicast => {
                    if step == 0 {
                        if response
                            .send(self.bind_unicast(self.interface).await)
                            .is_err()
                        {
                            // The sender has been dropped, so we should exit
                            return;
                        }
                        step += 1;
                    }
                }
                Control::UnbindUnicast => {
                    if step == 0 {
                        self.unbind_unicast().await;
                        if response.send(Ok(())).is_err() {
                            // The sender has been dropped, so we should exit
                            return;
                        }
                        step += 1;
                    }
                }
            }
        }
    }
    fn run(mut self) {
        tokio::spawn(async move {
            loop {
                let Self {
                    control_receiver,
                    discovery_receiver,
                    discovery_info,
                    update_sender,
                    ..
                } = &mut self;
                select! {
                    // Receive a control message
                    ctrl = control_receiver.recv() => {
                        if let Some(ctrl) = ctrl {
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
                Ok(message) => Ok(message),
                Err(err) => Err(Error::from(err)),
            }
        }
        Err(err) => Err(Error::from(err)),
    }
}
