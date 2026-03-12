mod error;
mod inner;
mod service_registry;
mod session;
mod socket_manager;

pub use error::Error;

use crate::{protocol, protocol::Message, traits::PayloadWireFormat};
use inner::{ControlMessage, Inner};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use tokio::sync::{mpsc, oneshot};
use tracing::info;

/// Handle to a pending SOME/IP request-response transaction.
/// Resolves when the inner loop receives a matching unicast reply.
/// Does not borrow `Client`.
pub struct PendingResponse<P> {
    receiver: oneshot::Receiver<Result<P, Error>>,
}

impl<P> std::fmt::Debug for PendingResponse<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingResponse").finish_non_exhaustive()
    }
}

impl<P> PendingResponse<P> {
    /// Await the response payload.
    ///
    /// # Errors
    ///
    /// Returns the same errors as the request itself (e.g. deserialization failure).
    ///
    /// # Panics
    ///
    /// Panics if the inner loop dropped the response channel.
    pub async fn response(self) -> Result<P, Error> {
        self.receiver
            .await
            .expect("inner loop dropped response channel")
    }
}

/// A discovery message together with its source address and SOME/IP header.
pub struct DiscoveryMessage<P: PayloadWireFormat> {
    /// The network address this discovery message was received from.
    pub source: SocketAddr,
    /// The SOME/IP header (contains `request_id` = `client_id` + `session_id`).
    pub someip_header: protocol::Header,
    /// The parsed SD header payload.
    pub sd_header: P::SdHeader,
}

impl<P: PayloadWireFormat> std::fmt::Debug for DiscoveryMessage<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscoveryMessage")
            .field("source", &self.source)
            .field("someip_header", &self.someip_header)
            .field("sd_header", &self.sd_header)
            .finish()
    }
}

/// An update received from the SOME/IP client event loop.
pub enum ClientUpdate<P: PayloadWireFormat> {
    /// Discovery message received.
    DiscoveryUpdated(DiscoveryMessage<P>),
    /// A remote sender has rebooted (detected via SD session tracking).
    SenderRebooted(SocketAddr),
    /// Unicast message received.
    Unicast(Message<P>),
    /// The client encountered an error.
    Error(Error),
}

impl<P: PayloadWireFormat> std::fmt::Debug for ClientUpdate<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DiscoveryUpdated(msg) => f.debug_tuple("DiscoveryUpdated").field(msg).finish(),
            Self::SenderRebooted(addr) => f.debug_tuple("SenderRebooted").field(addr).finish(),
            Self::Unicast(msg) => f.debug_tuple("Unicast").field(msg).finish(),
            Self::Error(err) => f.debug_tuple("Error").field(err).finish(),
        }
    }
}

/// A SOME/IP client that handles service discovery and message exchange.
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
    /// Creates a new client bound to the given network interface and spawns its event loop.
    #[must_use]
    pub fn new(interface: Ipv4Addr) -> Self {
        let (control_sender, update_receiver) = Inner::spawn(interface);

        Self {
            interface,
            control_sender,
            update_receiver,
        }
    }

    /// Waits for the next update from the client event loop.
    pub async fn run(&mut self) -> Option<ClientUpdate<MessageDefinitions>> {
        self.update_receiver.recv().await
    }

    /// Returns the current network interface address.
    #[must_use]
    pub fn interface(&self) -> Ipv4Addr {
        self.interface
    }

    /// Changes the network interface and rebinds sockets.
    ///
    /// # Errors
    ///
    /// Returns an error if rebinding sockets on the new interface fails.
    ///
    /// # Panics
    ///
    /// Panics if the internal control channel is closed.
    pub async fn set_interface(&mut self, interface: Ipv4Addr) -> Result<(), Error> {
        let (response, message) = ControlMessage::set_interface(interface);
        self.control_sender.send(message).await.unwrap();
        response.await.unwrap()?;
        self.interface = interface;
        Ok(())
    }

    /// Binds the SD multicast discovery socket.
    ///
    /// # Errors
    ///
    /// Returns an error if binding the multicast socket fails.
    ///
    /// # Panics
    ///
    /// Panics if the internal control channel is closed.
    pub async fn bind_discovery(&mut self) -> Result<(), Error> {
        let (response, message) = ControlMessage::bind_discovery();
        self.control_sender.send(message).await.unwrap();
        response.await.unwrap()
    }

    /// Unbinds the SD multicast discovery socket.
    ///
    /// # Errors
    ///
    /// Returns an error if unbinding the multicast socket fails.
    ///
    /// # Panics
    ///
    /// Panics if the internal control channel is closed.
    pub async fn unbind_discovery(&mut self) -> Result<(), Error> {
        let (response, message) = ControlMessage::unbind_discovery();
        self.control_sender.send(message).await.unwrap();
        response.await.unwrap()
    }

    /// Subscribes to an event group on a known service.
    ///
    /// # Errors
    ///
    /// Returns an error if the service is not found or subscription fails.
    ///
    /// # Panics
    ///
    /// Panics if the internal control channel is closed.
    pub async fn subscribe(
        &mut self,
        service_id: u16,
        instance_id: u16,
        major_version: u8,
        ttl: u32,
        event_group_id: u16,
        client_port: u16,
    ) -> Result<(), Error> {
        let (response, message) = ControlMessage::subscribe(
            service_id,
            instance_id,
            major_version,
            ttl,
            event_group_id,
            client_port,
        );
        self.control_sender.send(message).await.unwrap();
        response.await.unwrap()
    }

    /// Sends an SD message to a specific target address.
    ///
    /// # Errors
    ///
    /// Returns an error if sending the SD message fails.
    ///
    /// # Panics
    ///
    /// Panics if the internal control channel is closed.
    pub async fn send_sd_message(
        &mut self,
        target: SocketAddrV4,
        sd_header: <MessageDefinitions as PayloadWireFormat>::SdHeader,
    ) -> Result<(), Error> {
        let (response, message) = ControlMessage::send_sd(target, sd_header);
        self.control_sender.send(message).await.unwrap();
        response.await.unwrap()
    }

    /// Registers a service endpoint in the client's endpoint registry.
    ///
    /// # Errors
    ///
    /// Returns an error if registering the endpoint fails.
    ///
    /// # Panics
    ///
    /// Panics if the internal control channel is closed.
    pub async fn add_endpoint(
        &mut self,
        service_id: u16,
        instance_id: u16,
        addr: SocketAddrV4,
    ) -> Result<(), Error> {
        let (response, message) = ControlMessage::add_endpoint(service_id, instance_id, addr);
        self.control_sender.send(message).await.unwrap();
        response.await.unwrap()
    }

    /// Removes a service endpoint from the client's endpoint registry.
    ///
    /// # Errors
    ///
    /// Returns an error if removing the endpoint fails.
    ///
    /// # Panics
    ///
    /// Panics if the internal control channel is closed.
    pub async fn remove_endpoint(
        &mut self,
        service_id: u16,
        instance_id: u16,
    ) -> Result<(), Error> {
        let (response, message) = ControlMessage::remove_endpoint(service_id, instance_id);
        self.control_sender.send(message).await.unwrap();
        response.await.unwrap()
    }

    /// Sends a message to a service and returns a handle to await the response.
    ///
    /// The `&mut self` borrow is released once the UDP send completes.
    /// Call `.response()` on the returned handle to await the reply payload.
    ///
    /// # Errors
    ///
    /// Returns an error if the service is not found, unicast binding fails,
    /// or the UDP send fails.
    ///
    /// # Panics
    ///
    /// Panics if the internal control channel is closed.
    pub async fn send_to_service(
        &mut self,
        service_id: u16,
        instance_id: u16,
        message: crate::protocol::Message<MessageDefinitions>,
    ) -> Result<PendingResponse<MessageDefinitions>, Error> {
        let (send_rx, response_rx, ctrl_msg) =
            ControlMessage::send_to_service(service_id, instance_id, message);
        self.control_sender.send(ctrl_msg).await.unwrap();
        send_rx.await.unwrap()?;
        Ok(PendingResponse {
            receiver: response_rx,
        })
    }

    /// Shuts down the client, dropping the control channel and draining remaining updates.
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
    use crate::protocol::sd::test_support::{TestPayload, empty_sd_header};
    use crate::traits::WireFormat;
    use std::format;

    type TestClient = Client<TestPayload>;

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
        use std::net::SocketAddr;

        // DiscoveryUpdated
        let sd_header = empty_sd_header();
        let someip_header = crate::protocol::Header::new_sd(1, sd_header.required_size());
        let discovery_msg = DiscoveryMessage {
            source: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 30490),
            someip_header,
            sd_header,
        };
        let update: ClientUpdate<TestPayload> = ClientUpdate::DiscoveryUpdated(discovery_msg);
        let debug_str = format!("{update:?}");
        assert!(debug_str.contains("DiscoveryUpdated"));

        // SenderRebooted
        let update: ClientUpdate<TestPayload> =
            ClientUpdate::SenderRebooted(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 30490));
        let debug_str = format!("{update:?}");
        assert!(debug_str.contains("SenderRebooted"));

        // Unicast
        let msg = crate::protocol::Message::new_sd(1, &empty_sd_header());
        let update: ClientUpdate<TestPayload> = ClientUpdate::Unicast(msg);
        let debug_str = format!("{update:?}");
        assert!(debug_str.contains("Unicast"));

        // Error
        let update: ClientUpdate<TestPayload> = ClientUpdate::Error(Error::ServiceNotFound);
        let debug_str = format!("{update:?}");
        assert!(debug_str.contains("Error"));
    }

    #[tokio::test]
    async fn test_subscribe_unknown_service_returns_error() {
        let mut client = TestClient::new(Ipv4Addr::LOCALHOST);
        let result = client.subscribe(0xFFFF, 0xFFFF, 1, 3, 0x01, 0).await;
        assert!(
            matches!(result, Err(Error::ServiceNotFound)),
            "expected ServiceNotFound, got {result:?}"
        );
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
    async fn test_add_endpoint_succeeds() {
        let mut client = TestClient::new(Ipv4Addr::LOCALHOST);
        let addr = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 1), 30000);
        client.add_endpoint(0x1234, 0x0001, addr).await.unwrap();
        client.shut_down().await;
    }

    #[tokio::test]
    async fn test_send_to_service_unknown_returns_error() {
        let mut client = TestClient::new(Ipv4Addr::LOCALHOST);
        let msg = crate::protocol::Message::new_sd(1, &empty_sd_header());
        let result = client.send_to_service(0xFFFF, 0xFFFF, msg).await;
        assert!(
            matches!(result, Err(Error::ServiceNotFound)),
            "expected ServiceNotFound, got {result:?}"
        );
        client.shut_down().await;
    }

    #[tokio::test]
    async fn test_remove_endpoint_succeeds() {
        let mut client = TestClient::new(Ipv4Addr::LOCALHOST);
        let addr = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 1), 30000);
        client.add_endpoint(0x1234, 0x0001, addr).await.unwrap();
        client.remove_endpoint(0x1234, 0x0001).await.unwrap();
        client.shut_down().await;
    }

    #[test]
    fn test_pending_response_debug() {
        let (_tx, rx) = oneshot::channel::<Result<TestPayload, Error>>();
        let pending = PendingResponse { receiver: rx };
        let s = format!("{pending:?}");
        assert!(s.contains("PendingResponse"));
    }

    #[tokio::test]
    async fn test_pending_response_resolves_ok() {
        let (tx, rx) = oneshot::channel::<Result<TestPayload, Error>>();
        let pending = PendingResponse { receiver: rx };
        let payload = TestPayload {
            header: empty_sd_header(),
        };
        tx.send(Ok(payload.clone())).unwrap();
        let result = pending.response().await;
        assert_eq!(result.unwrap(), payload);
    }

    #[tokio::test]
    async fn test_pending_response_resolves_err() {
        let (tx, rx) = oneshot::channel::<Result<TestPayload, Error>>();
        let pending = PendingResponse { receiver: rx };
        tx.send(Err(Error::ServiceNotFound)).unwrap();
        let result = pending.response().await;
        assert!(
            matches!(result, Err(Error::ServiceNotFound)),
            "expected ServiceNotFound, got {result:?}"
        );
    }

    #[tokio::test]
    async fn test_send_sd_message() {
        let mut client = TestClient::new(Ipv4Addr::LOCALHOST);
        // Bind discovery first so the send path uses the existing socket
        client.bind_discovery().await.unwrap();
        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 30490);
        let sd_header = empty_sd_header();
        client.send_sd_message(target, sd_header).await.unwrap();
        client.shut_down().await;
    }

    #[tokio::test]
    async fn test_send_to_service_success_returns_pending_response() {
        let mut client = TestClient::new(Ipv4Addr::LOCALHOST);
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 30000);
        client.add_endpoint(0x1234, 0x0001, addr).await.unwrap();
        let msg = crate::protocol::Message::new_sd(1, &empty_sd_header());
        // send_to_service succeeds (send completes), returning a PendingResponse
        let pending = client.send_to_service(0x1234, 0x0001, msg).await;
        assert!(pending.is_ok());
        client.shut_down().await;
    }

    #[tokio::test]
    async fn test_run_returns_none_after_shutdown() {
        let mut client = TestClient::new(Ipv4Addr::LOCALHOST);
        // Drop the control sender by taking ownership, then check run() returns None
        let control_sender = client.control_sender.clone();
        drop(control_sender);
        // The original sender in client is still alive; drop it via shut_down
        // Instead, test that run() returns None when inner loop exits
        let cs = std::mem::replace(
            &mut client.control_sender,
            mpsc::channel::<inner::ControlMessage<TestPayload>>(1).0,
        );
        drop(cs);
        // Now the inner loop should exit; run() should return None
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(2), client.run()).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }
}
