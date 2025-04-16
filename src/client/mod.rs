mod inner;
mod socket_manager;

use crate::{
    Error,
    protocol::{Message, sd},
    traits::PayloadWireFormat,
};
use inner::{ControlMessage, Inner};
use std::net::{Ipv4Addr, SocketAddrV4};
use tokio::sync::mpsc;
use tracing::info;

#[derive(Debug)]
pub enum ClientUpdate<PayloadDefinitions> {
    /// Discovery message received
    DiscoveryUpdated(sd::Header),
    /// Unicast message received
    Unicast(Message<PayloadDefinitions>),
    /// Inner SOME/IP Client has encountered an error
    Error(Error),
}

#[derive(Debug)]
pub struct Client<MessageDefinitions> {
    interface: Ipv4Addr,
    control_sender: mpsc::Sender<inner::ControlMessage<MessageDefinitions>>,
    update_receiver: mpsc::Receiver<ClientUpdate<MessageDefinitions>>,
}

impl<MessageDefinitions> Client<MessageDefinitions>
where
    MessageDefinitions: PayloadWireFormat + Clone + std::fmt::Debug + 'static,
{
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
        let (response, message) = ControlMessage::bind_unicast();
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
        sd_header: crate::protocol::sd::Header,
    ) -> Result<(), Error> {
        let (response, message) = ControlMessage::send_sd(target, sd_header);
        self.control_sender.send(message).await.unwrap();
        response.await.unwrap()
    }

    pub async fn send_message(
        &mut self,
        target: SocketAddrV4,
        message: crate::protocol::Message<MessageDefinitions>,
    ) -> Result<MessageDefinitions, Error> {
        let (response, message) = ControlMessage::send_request(target, message);
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
