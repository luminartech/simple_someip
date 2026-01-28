use crate::{
    protocol::{Error, Message},
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

pub struct ClientConfig {
    pub client_ip: Ipv4Addr,
}

pub struct SomeIPClient {
    config: ClientConfig,
    discovery_socket: Option<UdpSocket>,
}

impl SomeIPClient {
    pub fn new(config: ClientConfig) -> Self {
        Self {
            config,
            discovery_socket: None,
        }
    }
    pub fn connect(&mut self) -> Result<(), Error> {
        let discovery_address = Ipv4Addr::from_str(SD_MULTICAST_IP).unwrap();
        let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 30490);
        let discover_socket = UdpSocket::bind(bind_addr)?;
        discover_socket
            .join_multicast_v4(&discovery_address, &self.config.client_ip)
            .unwrap();
        discover_socket.set_read_timeout(Some(Duration::from_millis(1)))?;
        println!("Successfully bound Discovery Socket");
        let mut rx_buffer = vec![0; 1400];
        loop {
            match discover_socket.recv(&mut rx_buffer) {
                Ok(_) => {
                    let message = Message::read(&mut rx_buffer.as_slice())?;
                    assert!(message.header().message_id.is_sd());
                    assert!(!message.header().message_type.is_tp());
                    println!("Received SD message: {:?}", message);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    continue;
                }
                Err(e) => {
                    return Err(Error::from(e));
                }
            }
        }
        Ok(())
    }
}
