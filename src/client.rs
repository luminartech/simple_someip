use tokio::{net::UdpSocket, select, sync::mpsc};

use crate::{
    Error, SD_MULTICAST_IP, SD_MULTICAST_PORT,
    protocol::{Message, sd},
    traits::{PayloadWireFormat, WireFormat},
};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

pub trait SomeIpMessageHandler {
    fn handle_message<PayloadDefinition: PayloadWireFormat>(
        &self,
        message: &Message<PayloadDefinition>,
    );
}

#[derive(Debug)]
pub struct SomeIPClient<MessageDefinitions> {
    discovery_receiver: Option<mpsc::Receiver<Option<sd::Header>>>,
    unicast_socket: Option<UdpSocket>,
    phantom: std::marker::PhantomData<MessageDefinitions>,
}

impl<MessageDefinitions: PayloadWireFormat> SomeIPClient<MessageDefinitions> {
    pub fn new() -> Self {
        Self {
            discovery_receiver: None,
            unicast_socket: None,
            phantom: std::marker::PhantomData,
        }
    }

    pub async fn bind_discovery_to_interface(&mut self, interface: Ipv4Addr) -> Result<(), Error> {
        let (sender, receiver) = mpsc::channel(16);
        let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), SD_MULTICAST_PORT);
        let discovery_socket = UdpSocket::bind(bind_addr).await?;
        tokio::spawn(async move {
            discovery_socket
                .join_multicast_v4(SD_MULTICAST_IP, interface)
                .unwrap();
            let mut buf = vec![0; 1400];
            loop {
                select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                        println!("Timeout");
                        if sender.send(None).await.is_err() {
                            // The receiver has been dropped, so we should exit
                            break;
                        }

                    }
                    Ok((_, _)) = discovery_socket.recv_from(&mut buf) => {
                        println!("Message received");
                        match Message::<MessageDefinitions>::from_reader(&mut buf.as_slice()) {
                            Ok(message) => {
                                let sd_header = message.get_sd_header().unwrap();
                                println!("Received SD message: {:?}", sd_header);
                                if sender.send(Some(sd_header.clone())).await.is_err() {
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
        self.discovery_receiver = Some(receiver);
        Ok(())
    }

    pub async fn run(&mut self) {
        if self.discovery_receiver.is_none() {
            panic!("Discovery receiver not bound");
        } else {
            let receiver = self.discovery_receiver.as_mut().unwrap();

            match receiver.recv().await {
                Some(Some(header)) => {}
                Some(None) => {}
                None => {
                    panic!("Discovery sender dropped");
                }
            }
        }
    }
}
/*
    pub fn unbind_discovery(&mut self) -> Result<(), Error> {
        if let Some(discovery_socket) = self.discovery_socket.as_ref() {
            discovery_socket.leave_multicast_v4(
                Ipv4Addr::from_str(SD_MULTICAST_IP).unwrap(),
                self.config.client_ip,
            )?;
        }
        self.discovery_socket = None;
        Ok(())
    }

    pub fn attempt_discovery(&mut self) -> Result<Option<sd::Header>, Error> {
        match self.discovery_socket.as_mut().unwrap().recv(buf) {
            Ok(packet_size) => {
                let message = Message::from_reader(&mut self.buffer.as_slice())?;
                assert!(message.header().message_id.is_sd());
                assert!(!message.header().message_type.is_tp());
                assert!(message.header().length as usize == packet_size - 8);
                if let MessagePayload::ServiceDiscovery(header) = message.payload() {
                    Ok(Some(header.to_owned()))
                } else {
                    Ok(None)
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(Error::from(e)),
        }
    }
}

    pub fn send_multicast_discovery_message<PayloadDefinition: PayloadWireFormat>(
        &self,
        message: &Message<PayloadDefinition>,
    ) -> Result<(), Error> {
        if self.discovery_socket.is_none() {
            return Err(Error::MulticastSocketNotConnected);
        }
        let mut buffer = Vec::new();
        message.to_writer(&mut buffer)?;
        let discovery_socket_addr =
            SocketAddr::new(IpAddr::from_str(SD_MULTICAST_IP).unwrap(), 30490);
        self.discovery_socket
            .as_ref()
            .unwrap()
            .send_to(&buffer, discovery_socket_addr)?;
        Ok(())
    }

    pub fn get_unicast_port(&self) -> Option<u16> {
        self.unicast_socket
            .as_ref()
            .map(|socket| socket.local_addr().unwrap().port())
    }

    pub fn send_unicast_discovery_message<PayloadDefinition: PayloadWireFormat>(
        &self,
        message: &Message<PayloadDefinition>,
        ip: Ipv4Addr,
    ) -> Result<(), Error> {
        if self.unicast_socket.is_none() {
            return Err(Error::UnicastSocketNotConnected);
        }
        let mut buffer = Vec::new();
        message.to_writer(&mut buffer)?;
        let discovery_socket_addr = SocketAddr::new(IpAddr::V4(ip), 30490);
        self.discovery_socket
            .as_ref()
            .unwrap()
            .send_to(&buffer, discovery_socket_addr)?;
        Ok(())
    }

    pub fn connect_unicast(&mut self, ip: Ipv4Addr, port: u16) -> Result<(), Error> {
        if self.unicast_socket.is_some() {
            return Ok(());
        }
        let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0);
        let target_addr = SocketAddr::new(IpAddr::V4(ip), port);
        let unicast_socket = UdpSocket::bind(bind_addr)?;
        unicast_socket.set_nonblocking(true)?;
        unicast_socket.connect(target_addr)?;
        self.unicast_socket = Some(unicast_socket);
        Ok(())
    }

    pub fn send_message(&self, message: &Message) -> Result<(), Error> {
        if self.unicast_socket.is_none() {
            return Err(Error::UnicastSocketNotConnected);
        }
        let mut buffer = Vec::new();
        message.write(&mut buffer)?;
        self.unicast_socket.as_ref().unwrap().send(&buffer)?;
        Ok(())
    }

    pub fn receive_message(&self) -> Result<Message, Error> {
        if self.unicast_socket.is_none() {
            return Err(Error::UnicastSocketNotConnected);
        }
        let mut buffer = [0; 1400];
        match self.unicast_socket.as_ref().unwrap().recv(&mut buffer) {
            Ok(packet_size) => {
                let message = Message::read(&mut buffer.as_ref())?;
                assert!(message.header().length as usize == packet_size - 8);
                Ok(message)
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => Err(Error::Timeout),
            Err(e) => Err(Error::from(e)),
        }
    }
}
*/
