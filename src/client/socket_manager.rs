use crate::{
    Error, SD_MULTICAST_IP, SD_MULTICAST_PORT,
    protocol::{Message, MessageView},
    traits::{PayloadWireFormat, WireFormat},
};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
    prelude::rust_2024::*,
    task::{Context, Poll},
    vec,
};
use tokio::{net::UdpSocket, select, sync::mpsc};
use tracing::{error, info, trace};

/// A received message together with the source address it came from.
#[derive(Clone, Debug)]
pub struct ReceivedMessage<P> {
    pub message: Message<P>,
    pub source: SocketAddr,
}

/// Structure representing a request to send a message
#[derive(Debug)]
pub struct SendMessage<PayloadDefinitions> {
    pub target_addr: SocketAddrV4,
    pub message: Message<PayloadDefinitions>,
    response: tokio::sync::oneshot::Sender<Result<(), Error>>,
}

impl<PayloadDefinitions: PayloadWireFormat + 'static> SendMessage<PayloadDefinitions> {
    pub fn new(
        target_addr: SocketAddrV4,
        message: Message<PayloadDefinitions>,
    ) -> (tokio::sync::oneshot::Receiver<Result<(), Error>>, Self) {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        (
            response_rx,
            Self {
                target_addr,
                message,
                response: response_tx,
            },
        )
    }
}

#[derive(Debug)]
pub struct SocketManager<PayloadDefinitions> {
    receiver: mpsc::Receiver<Result<ReceivedMessage<PayloadDefinitions>, Error>>,
    sender: mpsc::Sender<SendMessage<PayloadDefinitions>>,
    local_port: u16,
    session_id: u16,
}

impl<MessageDefinitions> SocketManager<MessageDefinitions>
where
    MessageDefinitions: PayloadWireFormat + 'static,
{
    pub fn bind_discovery(interface: Ipv4Addr) -> Result<Self, Error> {
        let (rx_tx, rx_rx) = mpsc::channel(16);
        let (tx_tx, tx_rx) = mpsc::channel(16);
        let bind_addr =
            std::net::SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), SD_MULTICAST_PORT);

        // Create socket with SO_REUSEADDR to allow quick restart
        let socket = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        socket.set_reuse_address(true)?;
        #[cfg(unix)]
        socket.set_reuse_port(true)?;
        socket.bind(&bind_addr.into())?;
        socket.set_nonblocking(true)?;
        let socket: std::net::UdpSocket = socket.into();
        let socket = UdpSocket::from_std(socket)?;

        socket.join_multicast_v4(SD_MULTICAST_IP, interface)?;

        Self::spawn_socket_loop(socket, rx_tx, tx_rx);
        Ok(Self {
            receiver: rx_rx,
            sender: tx_tx,
            local_port: SD_MULTICAST_PORT,
            session_id: 0,
        })
    }

    pub fn bind(port: u16) -> Result<Self, Error> {
        let (rx_tx, rx_rx) = mpsc::channel(4);
        let (tx_tx, tx_rx) = mpsc::channel(4);
        let bind_addr = std::net::SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);

        // Create socket with SO_REUSEADDR and SO_REUSEPORT to allow quick restart
        let socket = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        socket.set_reuse_address(true)?;
        socket.bind(&bind_addr.into())?;
        socket.set_nonblocking(true)?;
        let socket: std::net::UdpSocket = socket.into();
        let socket = UdpSocket::from_std(socket)?;
        let port = socket.local_addr()?.port();
        Self::spawn_socket_loop(socket, rx_tx, tx_rx);
        Ok(Self {
            receiver: rx_rx,
            sender: tx_tx,
            local_port: port,
            session_id: 0,
        })
    }

    pub async fn send(
        &mut self,
        target_addr: SocketAddrV4,
        message: Message<MessageDefinitions>,
    ) -> Result<(), Error> {
        let (result_channel, message) = SendMessage::new(target_addr, message);
        self.sender.send(message).await.map_err(|e| {
            error!("Socket error: {e} when attempting to send message");
            Error::SocketClosedUnexpectedly
        })?;
        result_channel
            .await
            .expect("Socket manager must always return result of send before dropping channel")?;
        self.session_id += 1;
        Ok(())
    }

    pub async fn receive(&mut self) -> Option<Result<ReceivedMessage<MessageDefinitions>, Error>> {
        self.receiver.recv().await
    }

    /// Poll the receiver for a message without blocking.
    /// Used by `Inner::receive_any_unicast` to poll multiple sockets.
    pub fn poll_receive(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<ReceivedMessage<MessageDefinitions>, Error>>> {
        self.receiver.poll_recv(cx)
    }

    pub fn session_id(&self) -> u16 {
        self.session_id
    }

    pub fn port(&self) -> u16 {
        self.local_port
    }

    pub async fn shut_down(self) {
        let Self {
            sender,
            mut receiver,
            ..
        } = self;
        drop(sender);
        _ = receiver.recv().await;
    }

    fn spawn_socket_loop(
        socket: UdpSocket,
        rx_tx: mpsc::Sender<Result<ReceivedMessage<MessageDefinitions>, Error>>,
        mut tx_rx: mpsc::Receiver<SendMessage<MessageDefinitions>>,
    ) {
        tokio::spawn(async move {
            let mut buf = vec![0; 1400];
            loop {
                select! {
                    result = socket.recv_from(&mut buf) => {
                        match result {
                            Ok((bytes_received, source_address)) => {
                                let parse_result = MessageView::parse(&buf[..bytes_received])
                                    .and_then(|view| {
                                        let header = view.header().to_owned();
                                        let payload = MessageDefinitions::from_payload_bytes(header.message_id, view.payload_bytes())?;
                                        Ok(ReceivedMessage {
                                            message: Message::new(header, payload),
                                            source: source_address,
                                        })
                                    })
                                    .map_err(Error::from);
                                if let Ok(()) = rx_tx.send( parse_result ).await {} else {
                                    info!("Socket Dropping");
                                    // The receiver has been dropped, so we should exit
                                    break;
                                }
                            }
                            Err(e) => {

                                error!("Error decoding message: {:?}", e);
                            }
                        }
                    },
                    message = tx_rx.recv() => {
                        if let Some(send_message) = message {
                            trace!("Sending: {:?}", &send_message);
                            let message_length = match send_message.message.encode(&mut buf.as_mut_slice()) {
                                Ok(length) => length,
                                Err(e) => {
                                    error!("Failed to encode message: {:?}", e);
                                    // If the sender is already closed we can't send the error back, so we shut everything down
                                    if let Ok(()) = send_message.response.send(Err(e.into())) {
                                        // Successfully sent error back to sender, carry on
                                        continue;
                                    }
                                    error!("Socket owner closed channel unexpectedly, closing socket.");
                                    break;
                                }
                            };
                            match socket.send_to(&buf[..message_length], send_message.target_addr).await {
                                Ok(_bytes_sent) => {
                                    trace!("Sent {} bytes to {}", message_length, send_message.target_addr);
                                    if let Ok(()) = send_message.response.send(Ok(())) {} else {
                                        info!("Socket owner closed channel, closing socket.");
                                        // The sender has been dropped, so we should exit
                                        break;
                                    }
                                }
                                Err(e) => {
                                    error!("Failed to send message with error: {:?}", e);
                                    if let Ok(()) = send_message.response.send(Err(Error::Io(e))) {  } else {
                                        error!("Socket owner closed channel unexpectedly, closing socket.");
                                        break;
                                    }
                                }
                            }
                        } else {
                            info!("Send channel closed, closing socket.");
                            // The sender has been dropped, so we should exit
                            break;
                        }
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::DiscoveryOnlyPayload;

    type TestSocketManager = SocketManager<DiscoveryOnlyPayload>;

    #[tokio::test]
    async fn test_bind_ephemeral_port() {
        let sm = TestSocketManager::bind(0).unwrap();
        assert!(sm.port() > 0);
        assert_eq!(sm.session_id(), 0);
    }

    #[tokio::test]
    async fn test_send_message_new() {
        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1234);
        let msg = Message::new_sd(
            1,
            &crate::protocol::sd::Header::new_find_services(false, &[]),
        );
        let (rx, send_msg) = SendMessage::<DiscoveryOnlyPayload>::new(target, msg);
        assert_eq!(send_msg.target_addr, target);
        // Verify the oneshot channel works
        send_msg.response.send(Ok(())).unwrap();
        assert!(rx.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn test_socket_manager_shut_down() {
        let sm = TestSocketManager::bind(0).unwrap();
        sm.shut_down().await;
    }

    #[tokio::test]
    async fn test_socket_manager_send_and_receive() {
        let mut sm = TestSocketManager::bind(0).unwrap();
        let sm_port = sm.port();

        // Create a raw UDP socket to send data to the SocketManager
        let raw_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        // Build and encode an SD message
        let sd_header = crate::protocol::sd::Header::new_find_services(false, &[]);
        let msg = Message::<DiscoveryOnlyPayload>::new_sd(1, &sd_header);
        let mut buf = vec![0u8; 128];
        let n = msg.encode(&mut buf.as_mut_slice()).unwrap();

        // Send raw bytes to the SocketManager's port
        raw_socket
            .send_to(&buf[..n], SocketAddrV4::new(Ipv4Addr::LOCALHOST, sm_port))
            .await
            .unwrap();

        // Receive the decoded message from the SocketManager
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), sm.receive())
            .await
            .expect("Timed out waiting for message");

        let received = result.unwrap().unwrap();
        assert_eq!(
            received.message.header().message_id,
            msg.header().message_id
        );
        assert!(received.message.is_sd());
    }

    #[tokio::test]
    async fn test_socket_manager_send_to_target() {
        let mut sm = TestSocketManager::bind(0).unwrap();

        // Create a raw socket to receive
        let raw_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let raw_port = raw_socket.local_addr().unwrap().port();

        let sd_header = crate::protocol::sd::Header::new_find_services(false, &[]);
        let msg = Message::<DiscoveryOnlyPayload>::new_sd(1, &sd_header);
        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, raw_port);

        sm.send(target, msg.clone()).await.unwrap();
        assert_eq!(sm.session_id(), 1);

        // Verify the raw socket received data
        let mut recv_buf = vec![0u8; 1400];
        let (len, _addr) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            raw_socket.recv_from(&mut recv_buf),
        )
        .await
        .expect("Timed out waiting for sent data")
        .unwrap();

        // Decode and verify
        let view = MessageView::parse(&recv_buf[..len]).unwrap();
        assert_eq!(view.header().to_owned().message_id, msg.header().message_id);
    }
}
