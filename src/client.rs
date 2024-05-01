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
    pub read_timeout: Option<Duration>,
}

#[derive(Debug)]
pub struct SomeIPClient {
    config: ClientConfig,
    discovery_socket: Option<UdpSocket>,
    buffer: [u8; 1400],
}

impl SomeIPClient {
    pub fn new(config: ClientConfig) -> Self {
        Self {
            config,
            discovery_socket: None,
            buffer: [0; 1400],
        }
    }

    pub fn bind_discovery(&mut self) -> Result<(), Error> {
        let discovery_address = Ipv4Addr::from_str(SD_MULTICAST_IP).unwrap();
        let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 30490);
        let discovery_socket = UdpSocket::bind(bind_addr)?;
        discovery_socket
            .join_multicast_v4(&discovery_address, &self.config.client_ip)
            .unwrap();
        discovery_socket.set_read_timeout(self.config.read_timeout)?;
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
}
