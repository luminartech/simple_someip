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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::DiscoveryOnlyPayload;
    use std::format;

    type TestClient = Client<DiscoveryOnlyPayload>;

    #[tokio::test]
    async fn test_client_new_and_interface() {
        let client = TestClient::new(Ipv4Addr::LOCALHOST);
        assert_eq!(client.interface(), Ipv4Addr::LOCALHOST);
        client.shut_down().await;
    }

    #[tokio::test]
    async fn test_client_debug() {
        let client = TestClient::new(Ipv4Addr::LOCALHOST);
        let debug_str = format!("{client:?}");
        assert!(debug_str.contains("Client"));
        assert!(debug_str.contains("127.0.0.1"));
        client.shut_down().await;
    }

    #[tokio::test]
    async fn test_client_update_debug() {
        use crate::protocol::sd;

        // DiscoveryUpdated
        let sd_header = sd::Header::new_find_services(false, &[]);
        let update: ClientUpdate<DiscoveryOnlyPayload> = ClientUpdate::DiscoveryUpdated(sd_header);
        let debug_str = format!("{update:?}");
        assert!(debug_str.contains("DiscoveryUpdated"));

        // Unicast
        let msg = crate::protocol::Message::new_sd(1, &sd::Header::new_find_services(false, &[]));
        let update: ClientUpdate<DiscoveryOnlyPayload> = ClientUpdate::Unicast(msg);
        let debug_str = format!("{update:?}");
        assert!(debug_str.contains("Unicast"));

        // Error
        let update: ClientUpdate<DiscoveryOnlyPayload> =
            ClientUpdate::Error(Error::UnicastSocketNotBound);
        let debug_str = format!("{update:?}");
        assert!(debug_str.contains("Error"));
    }

    #[tokio::test]
    async fn test_bind_unbind_unicast() {
        let mut client = TestClient::new(Ipv4Addr::LOCALHOST);
        let port = client.bind_unicast().await.unwrap();
        assert!(port > 0);
        client.unbind_unicast().await.unwrap();
        client.shut_down().await;
    }

    #[tokio::test]
    async fn test_bind_unicast_with_port_none() {
        let mut client = TestClient::new(Ipv4Addr::LOCALHOST);
        let port = client.bind_unicast_with_port(None).await.unwrap();
        assert!(port > 0);
        client.shut_down().await;
    }

    #[tokio::test]
    async fn test_bind_discovery_and_unbind() {
        let mut client = TestClient::new(Ipv4Addr::LOCALHOST);
        client.bind_discovery().await.unwrap();
        client.unbind_discovery().await.unwrap();
        client.shut_down().await;
    }

    #[tokio::test]
    async fn test_set_interface() {
        let mut client = TestClient::new(Ipv4Addr::LOCALHOST);
        let new_addr = Ipv4Addr::LOCALHOST;
        client.set_interface(new_addr).await.unwrap();
        assert_eq!(client.interface(), new_addr);
        client.shut_down().await;
    }

    #[tokio::test]
    async fn test_send_message_no_unicast_bound() {
        let mut client = TestClient::new(Ipv4Addr::LOCALHOST);
        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 12345);
        let msg = crate::protocol::Message::new_sd(
            1,
            &crate::protocol::sd::Header::new_find_services(false, &[]),
        );
        let result = client.send_message(target, msg, 9999).await;
        assert!(result.is_err());
        client.shut_down().await;
    }
}
