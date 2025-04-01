mod discovery_info;
pub use discovery_info::{DiscoveredIpV4Endpoint, DiscoveryInfo, EndpointInfo};
mod inner;

use inner::{Control, ControlMessage, Inner};
use tokio::sync::mpsc;

use crate::{Error, protocol, traits::PayloadWireFormat};
use std::net::Ipv4Addr;

#[derive(Debug)]
pub enum ClientUpdate<MessageDefinitions> {
    DiscoveryUpdated(DiscoveryInfo),
    Unicast(MessageDefinitions),
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

    pub async fn run(&mut self) -> ClientUpdate<PayloadDefinitions> {
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

    pub async fn send_sd_message(
        &mut self,
        sd_header: crate::protocol::sd::Header,
    ) -> Result<(), Error> {
        self.send_control_message(Control::SendSD(sd_header)).await
    }

    async fn send_control_message(
        &mut self,
        control: Control<PayloadDefinitions>,
    ) -> Result<(), Error> {
        let (control_message, response_sender) = ControlMessage::new(control);
        self.control_sender.send(control_message).await.unwrap();
        response_sender.await.unwrap()
    }
}
