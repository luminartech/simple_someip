mod inner;

use inner::{Control, ControlMessage, Inner};
use tokio::sync::mpsc;

use crate::{Error, protocol::sd, traits::PayloadWireFormat};
use std::net::Ipv4Addr;

#[derive(Debug)]
pub enum ClientUpdate<MessageDefinitions> {
    DiscoveryUpdated(sd::Header),
    Unicast(MessageDefinitions),
    Error(Error),
}

#[derive(Debug)]
pub struct Client<MessageDefinitions> {
    interface: Ipv4Addr,
    unicast_port: Option<u16>,
    control_sender: mpsc::Sender<inner::ControlMessage>,
    update_receiver: mpsc::Receiver<ClientUpdate<MessageDefinitions>>,
    phantom: std::marker::PhantomData<MessageDefinitions>,
}

impl<MessageDefinitions> Client<MessageDefinitions>
where
    MessageDefinitions: PayloadWireFormat + Clone + std::fmt::Debug + 'static,
{
    pub fn new(interface: Ipv4Addr) -> Self {
        let (control_sender, update_receiver) = Inner::new(interface);

        Self {
            interface,
            unicast_port: None,
            control_sender,
            update_receiver,
            phantom: std::marker::PhantomData,
        }
    }

    pub async fn run(&mut self) -> ClientUpdate<MessageDefinitions> {
        self.update_receiver.recv().await.unwrap()
    }

    pub fn interface(&self) -> Ipv4Addr {
        self.interface
    }

    pub async fn set_interface(&mut self, interface: Ipv4Addr) -> Result<(), Error> {
        self.send_control_message(Control::SetInterface(interface))
            .await?;
        self.interface = interface;
        Ok(())
    }

    pub async fn bind_discovery(&mut self) -> Result<(), Error> {
        self.send_control_message(Control::BindDiscovery).await
    }

    pub async fn unbind_discovery(&mut self) -> Result<(), Error> {
        self.send_control_message(inner::Control::UnbindDiscovery)
            .await
    }

    async fn send_control_message(&mut self, control: Control) -> Result<(), Error> {
        let (control_message, response_sender) = ControlMessage::new(control);
        self.control_sender.send(control_message).await.unwrap();
        response_sender.await.unwrap()
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
