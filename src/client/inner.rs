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

pub(super) enum Control {
    SetInterface(Ipv4Addr),
    BindDiscovery,
    UnbindDiscovery,
}

pub(super) struct ControlMessage {
    control: Control,
    response: oneshot::Sender<Result<(), Error>>,
}

impl ControlMessage {
    pub fn new(control: Control) -> (Self, oneshot::Receiver<Result<(), Error>>) {
        let (response, receiver) = oneshot::channel();
        (Self { control, response }, receiver)
    }
}

#[derive(Debug)]
struct UnicastInfo<MessageDefinitions> {
    receiver: mpsc::Receiver<Option<Message<MessageDefinitions>>>,
    port: u16,
}

#[derive(Debug)]
pub(super) struct Inner<MessageDefinitions> {
    control_receiver: Receiver<ControlMessage>,
    update_sender: mpsc::Sender<ClientUpdate<MessageDefinitions>>,
    interface: Ipv4Addr,
    discovery_receiver: Option<mpsc::Receiver<Result<Message<MessageDefinitions>, Error>>>,
    discovery_info: DiscoveryInfo,
    unicast_info: Option<UnicastInfo<MessageDefinitions>>,
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

    async fn bind_unicast_to_interface(&mut self, interface: Ipv4Addr) -> Result<(), Error> {
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

                    ctrl = control_receiver.recv() => {
                        if let Some( ControlMessage{ control, response }) = ctrl {
                            match control {
                                Control::SetInterface(interface) => {
                                    self.interface = interface;
                                    response.send(Ok(())).unwrap();
                                }
                                Control::BindDiscovery => {
                                    response.send(self.bind_discovery().await).unwrap();
                                }
                                Control::UnbindDiscovery => {
                                    self.unbind_discovery();
                                    response.send(Ok(())).unwrap();
                                }
                            }
                        } else {
                            // The sender has been dropped, so we should exit
                            break;
                        }
                    }
                    discovery = Inner::receive_discovery(discovery_receiver) => {
                        match discovery {
                            Ok(header) => {
                                match discovery_info.update(header) {
                                    Ok(info) => {
                                        update_sender.send(ClientUpdate::DiscoveryUpdated(info)).await.unwrap();
                                    }
                                    Err(err) => {
                                        update_sender.send(ClientUpdate::Error(err)).await.unwrap();
                                    }
                                }
                            }
                            Err(err) => {
                                update_sender.send(ClientUpdate::Error(err)).await.unwrap();
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
