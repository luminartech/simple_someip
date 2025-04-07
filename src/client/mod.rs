mod inner;
mod socket_manager;

pub use inner::ControlResponse;

use crate::{Error, protocol::sd, traits::PayloadWireFormat};
use inner::{Control, ControlMessage, Inner};
use std::net::{Ipv4Addr, SocketAddrV4};
use tokio::sync::mpsc;

#[derive(Debug)]
pub enum ClientUpdate<MessageDefinitions> {
    /// Discovery message received
    DiscoveryUpdated(sd::Header),
    /// Unicast message received
    Unicast(MessageDefinitions),
    /// Inner SOME/IP Client has encountered an error
    Error(Error),
}

#[derive(Debug)]
pub struct Client<MessageDefinitions> {
    interface: Ipv4Addr,
    control_sender: mpsc::Sender<inner::ControlMessage<MessageDefinitions>>,
    update_receiver: mpsc::Receiver<ClientUpdate<MessageDefinitions>>,
}

impl<PayloadDefinitions> Client<PayloadDefinitions>
where
    PayloadDefinitions: PayloadWireFormat + Clone + std::fmt::Debug + 'static,
{
    pub fn new(interface: Ipv4Addr) -> Self {
        let (control_sender, update_receiver) = Inner::new(interface);

        Self {
            interface,
            control_sender,
            update_receiver,
        }
    }

    pub async fn run(&mut self) -> Option<ClientUpdate<PayloadDefinitions>> {
        self.update_receiver.recv().await
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

    pub async fn bind_discovery(&mut self) -> Result<ControlResponse, Error> {
        self.send_control_message(Control::BindDiscovery).await
    }

    pub async fn unbind_discovery(&mut self) -> Result<ControlResponse, Error> {
        self.send_control_message(inner::Control::UnbindDiscovery)
            .await
    }

    pub async fn bind_unicast(&mut self) -> Result<ControlResponse, Error> {
        self.send_control_message(Control::BindUnicast).await
    }

    pub async fn unbind_unicast(&mut self) -> Result<ControlResponse, Error> {
        self.send_control_message(Control::UnbindUnicast).await
    }

    pub async fn send_sd_message(
        &mut self,
        target: SocketAddrV4,
        sd_header: &crate::protocol::sd::Header,
    ) -> Result<ControlResponse, Error> {
        self.send_control_message(Control::SendSD(target, sd_header.to_owned()))
            .await
    }

    async fn send_control_message(
        &mut self,
        control: Control<PayloadDefinitions>,
    ) -> Result<ControlResponse, Error> {
        let (control_message, response_sender) = ControlMessage::new(control);
        self.control_sender.send(control_message).await.unwrap();
        response_sender.await.unwrap() // The inner client should always respond
    }
}
