use crate::{
    protocol::{sd, Error, Message, MessagePayload},
    SD_MULTICAST_IP,
};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    str::FromStr,
    time::Duration,
};

pub trait SomeIpMessageHandler {
    fn handle_message(&self, message: &Message);
}

#[derive(Debug)]
pub struct ClientConfig {
    pub client_ip: Ipv4Addr,
}

#[derive(Debug)]
pub struct SomeIPClient {
    pub config: ClientConfig,
    discovery_socket: Option<UdpSocket>,
    unicast_socket: Option<UdpSocket>,
    buffer: [u8; 1400],
}

impl SomeIPClient {
    pub fn new(config: ClientConfig) -> Self {
        Self {
            config,
            discovery_socket: None,
            unicast_socket: None,
            buffer: [0; 1400],
        }
    }

    pub fn bind_discovery(&mut self) -> Result<(), Error> {
        let discovery_address = Ipv4Addr::from_str(SD_MULTICAST_IP).unwrap();
        let bind_addr = SocketAddr::new(IpAddr::V4(self.config.client_ip), 30490);
        let discovery_socket = UdpSocket::bind(bind_addr)?;

        discovery_socket
            .join_multicast_v4(&discovery_address, &self.config.client_ip)
            .unwrap();
        discovery_socket.set_nonblocking(true)?;
        self.discovery_socket = Some(discovery_socket);
        Ok(())
    }

    pub fn unbind_discovery(&mut self) -> Result<(), Error> {
        if let Some(discovery_socket) = self.discovery_socket.as_ref() {
            discovery_socket.leave_multicast_v4(
                &Ipv4Addr::from_str(SD_MULTICAST_IP).unwrap(),
                &self.config.client_ip,
            )?;
        }
        self.discovery_socket = None;
        Ok(())
    }

    pub fn send_multicast_discovery_message(&self, message: &Message) -> Result<(), Error> {
        if self.discovery_socket.is_none() {
            return Err(Error::MulticastSocketNotConnected);
        }
        let mut buffer = Vec::new();
        message.write(&mut buffer)?;
        let discovery_socket_addr =
            SocketAddr::new(IpAddr::from_str(SD_MULTICAST_IP).unwrap(), 30490);
        self.discovery_socket
            .as_ref()
            .unwrap()
            .send_to(&buffer, discovery_socket_addr)?;
        Ok(())
    }

    pub fn send_unicast_discovery_message(
        &self,
        message: &Message,
        ip: Ipv4Addr,
    ) -> Result<(), Error> {
        if self.discovery_socket.is_none() {
            return Err(Error::MulticastSocketNotConnected);
        }
        let mut buffer = Vec::new();
        message.write(&mut buffer)?;
        let discovery_socket_addr = SocketAddr::new(IpAddr::V4(ip), 30490);
        self.discovery_socket
            .as_ref()
            .unwrap()
            .send_to(&buffer, discovery_socket_addr)?;
        Ok(())
    }

    pub fn attempt_discovery(&mut self) -> Result<Option<sd::Header>, Error> {
        match self
            .discovery_socket
            .as_mut()
            .unwrap()
            .recv(&mut self.buffer)
        {
            Ok(packet_size) => {
                let message = Message::read(&mut self.buffer.as_slice())?;
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

    pub fn connect_unicast(&mut self, ip: Ipv4Addr, port: u16) -> Result<(), Error> {
        let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 10, 87)), 0);
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
