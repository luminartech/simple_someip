use crate::{
    Error, SD_MULTICAST_IP, SD_MULTICAST_PORT,
    protocol::Message,
    traits::{PayloadWireFormat, WireFormat},
};
use std::net::{IpAddr, Ipv4Addr, SocketAddrV4};
use tokio::{net::UdpSocket, select, sync::mpsc};
use tracing::{error, info, trace};

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
    receiver: mpsc::Receiver<Result<Message<PayloadDefinitions>, Error>>,
    sender: mpsc::Sender<SendMessage<PayloadDefinitions>>,
    local_port: u16,
    session_id: u16,
}

impl<MessageDefinitions> SocketManager<MessageDefinitions>
where
    MessageDefinitions: PayloadWireFormat + 'static,
{
    pub async fn bind_discovery(interface: Ipv4Addr) -> Result<Self, Error> {
        let (rx_tx, rx_rx) = mpsc::channel(16);
        let (tx_tx, tx_rx) = mpsc::channel(16);
        let bind_addr =
            std::net::SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), SD_MULTICAST_PORT);
        
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

        socket.join_multicast_v4(SD_MULTICAST_IP, interface)?;

        Self::spawn_socket_loop(socket, rx_tx, tx_rx);
        Ok(Self {
            receiver: rx_rx,
            sender: tx_tx,
            local_port: SD_MULTICAST_PORT,
            session_id: 0,
        })
    }

    pub async fn bind(port: u16) -> Result<Self, Error> {
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

    pub async fn receive(&mut self) -> Option<Result<Message<MessageDefinitions>, Error>> {
        self.receiver.recv().await
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
        rx_tx: mpsc::Sender<Result<Message<MessageDefinitions>, Error>>,
        mut tx_rx: mpsc::Receiver<SendMessage<MessageDefinitions>>,
    ) {
        tokio::spawn(async move {
            let mut buf = vec![0; 1400];
            loop {
                select! {
                    result = socket.recv_from(&mut buf) => {
                        match result {
                            Ok((_bytes_received, _source_address )) => {
                                let parse_result = Message::<MessageDefinitions>::decode(&mut buf.as_slice()).map_err(Error::from);
                                match rx_tx.send( parse_result ).await {
                                    Ok(_) => {}
                                    Err(_) => {
                                        info!("Socket Dropping");
                                        // The receiver has been dropped, so we should exit
                                        break;
                                    }
                                }
                            }
                            Err(e) => {

                                error!("Error decoding message: {:?}", e)
                            }
                        }
                    },
                    message = tx_rx.recv() => {
                        match message {
                            Some(send_message) => {
                                trace!("Sending: {:?}", &send_message);
                                let message_length = match send_message.message.encode(&mut buf.as_mut_slice()) {
                                    Ok(length) => length,
                                    Err(e) => {
                                        error!("Failed to encode message: {:?}", e);
                                        // If the sender is already closed we can't send the error back, so we shut everything down
                                        match send_message.response.send(Err(e.into())) {
                                            Ok(_) => {
                                                // Successfully sent error back to sender, carry on
                                                continue;
                                            }
                                            Err(_) => {
                                                error!("Socket owner closed channel unexpectedly, closing socket.");
                                                break;
                                            }
                                        }
                                    }
                                };
                                match socket.send_to(&buf[..message_length], send_message.target_addr).await {
                                    Ok(_bytes_sent) => {
                                        trace!("Sent {} bytes to {}", message_length, send_message.target_addr);
                                        match send_message.response.send(Ok(())) {
                                            Ok(_) => {}
                                            Err(_) => {
                                                info!("Socket owner closed channel, closing socket.");
                                                // The sender has been dropped, so we should exit
                                                break;
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        error!("Failed to send message with error: {:?}", e);
                                        match send_message.response.send(Err(Error::Io(e)))
                                        {
                                            Ok(())=> (),
                                            Err(_)=> {
                                                error!("Socket owner closed channel unexpectedly, closing socket.");
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                            None => {
                                info!("Send channel closed, closing socket.");
                                // The sender has been dropped, so we should exit
                                break;
                            }
                        }
                    }
                }
            }
        });
    }
}
