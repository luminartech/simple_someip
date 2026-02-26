mod inner;
mod socket_manager;

use crate::{Error, protocol::Message, traits::PayloadWireFormat};
use inner::{ControlMessage, Inner};
use std::net::{Ipv4Addr, SocketAddrV4};
use tokio::sync::mpsc;
use tracing::info;

pub enum ClientUpdate<P: PayloadWireFormat> {
    /// Discovery message received
    DiscoveryUpdated(P::SdHeader),
    /// Unicast message received
    Unicast(Message<P>),
    /// Inner SOME/IP Client has encountered an error
    Error(Error),
}

impl<P: PayloadWireFormat> std::fmt::Debug for ClientUpdate<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DiscoveryUpdated(header) => {
                f.debug_tuple("DiscoveryUpdated").field(header).finish()
            }
            Self::Unicast(msg) => f.debug_tuple("Unicast").field(msg).finish(),
            Self::Error(err) => f.debug_tuple("Error").field(err).finish(),
        }
    }
}

pub struct Client<MessageDefinitions: PayloadWireFormat> {
    interface: Ipv4Addr,
    control_sender: mpsc::Sender<inner::ControlMessage<MessageDefinitions>>,
    update_receiver: mpsc::Receiver<ClientUpdate<MessageDefinitions>>,
}

impl<MessageDefinitions: PayloadWireFormat> std::fmt::Debug for Client<MessageDefinitions> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("interface", &self.interface)
            .finish_non_exhaustive()
    }
}

impl<MessageDefinitions> Client<MessageDefinitions>
where
    MessageDefinitions: PayloadWireFormat + Clone + std::fmt::Debug + 'static,
{
    #[must_use]
    pub fn new(interface: Ipv4Addr) -> Self {
        let (control_sender, update_receiver) = Inner::spawn(interface);

        Self {
            interface,
            control_sender,
            update_receiver,
        }
    }

    pub async fn run(&mut self) -> Option<ClientUpdate<MessageDefinitions>> {
        self.update_receiver.recv().await
    }

    #[must_use]
    pub fn interface(&self) -> Ipv4Addr {
        self.interface
    }

    pub async fn set_interface(&mut self, interface: Ipv4Addr) -> Result<(), Error> {
        let (response, message) = ControlMessage::set_interface(interface);
        self.control_sender.send(message).await.unwrap();
        response.await.unwrap()?;
        self.interface = interface;
        Ok(())
    }

    pub async fn bind_discovery(&mut self) -> Result<(), Error> {
        let (response, message) = ControlMessage::bind_discovery();
        self.control_sender.send(message).await.unwrap();
        response.await.unwrap()
    }

    pub async fn unbind_discovery(&mut self) -> Result<(), Error> {
        let (response, message) = ControlMessage::unbind_discovery();
        self.control_sender.send(message).await.unwrap();
        response.await.unwrap()
    }

    pub async fn bind_unicast(&mut self) -> Result<u16, Error> {
        self.bind_unicast_with_port(None).await
    }

    pub async fn bind_unicast_with_port(&mut self, port: Option<u16>) -> Result<u16, Error> {
        let (response, message) = ControlMessage::bind_unicast_with_port(port);
        self.control_sender.send(message).await.unwrap();
        response.await.unwrap()
    }

    pub async fn unbind_unicast(&mut self) -> Result<(), Error> {
        let (response, message) = ControlMessage::unbind_unicast();
        self.control_sender.send(message).await.unwrap();
        response.await.unwrap()
    }

    pub async fn send_sd_message(
        &mut self,
        target: SocketAddrV4,
        sd_header: <MessageDefinitions as PayloadWireFormat>::SdHeader,
    ) -> Result<(), Error> {
        let (response, message) = ControlMessage::send_sd(target, sd_header);
        self.control_sender.send(message).await.unwrap();
        response.await.unwrap()
    }

    pub async fn send_message(
        &mut self,
        target: SocketAddrV4,
        message: crate::protocol::Message<MessageDefinitions>,
        source_port: u16,
    ) -> Result<MessageDefinitions, Error> {
        let (response, message) = ControlMessage::send_request(target, message, source_port);
        self.control_sender.send(message).await.unwrap();
        response.await.unwrap()
    }

    pub async fn shut_down(self) {
        let Self {
            control_sender,
            mut update_receiver,
            ..
        } = self;
        drop(control_sender);
        info!("Shutting Down SOME/IP client");
        while update_receiver.recv().await.is_some() {
            info!(".");
        }
    }
}
