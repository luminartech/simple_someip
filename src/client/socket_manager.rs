use std::net::{IpAddr, Ipv4Addr, SocketAddrV4};

use tokio::{net::UdpSocket, select, sync::mpsc};
use tracing::{error, info, trace};

use crate::{
    Error, SD_MULTICAST_IP, SD_MULTICAST_PORT,
    protocol::Message,
    traits::{PayloadWireFormat, WireFormat},
};

use super::inner::ControlResponse;

#[derive(Debug)]
pub struct SocketManager<PayloadDefinitions> {
    receiver: mpsc::Receiver<Result<Message<PayloadDefinitions>, Error>>,
    sender: mpsc::Sender<(SocketAddrV4, Message<PayloadDefinitions>)>,
    local_port: u16,
    session_id: u16,
}

impl<PayloadDefinitions> SocketManager<PayloadDefinitions>
where
    PayloadDefinitions: PayloadWireFormat + 'static,
{
    pub async fn bind_discovery(interface: Ipv4Addr) -> Result<Self, Error> {
        let (rx_tx, rx_rx) = mpsc::channel(16);
        let (tx_tx, tx_rx) = mpsc::channel(16);
        let bind_addr =
            std::net::SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), SD_MULTICAST_PORT);
        let socket = UdpSocket::bind(bind_addr).await?;

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
        let (rx_tx, rx_rx) = mpsc::channel(16);
        let (tx_tx, tx_rx) = mpsc::channel(16);
        let bind_addr = std::net::SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
        let socket = UdpSocket::bind(bind_addr).await?;
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
        message: Message<PayloadDefinitions>,
    ) -> Result<ControlResponse, Error> {
        self.sender
            .send((target_addr, message))
            .await
            .map_err(|_| Error::SocketClosedUnexpectedly)?;
        self.session_id += 1;
        Ok(ControlResponse::Success)
    }

    pub async fn receive(&mut self) -> Option<Result<Message<PayloadDefinitions>, Error>> {
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
        rx_tx: mpsc::Sender<Result<Message<PayloadDefinitions>, Error>>,
        mut tx_rx: mpsc::Receiver<(SocketAddrV4, Message<PayloadDefinitions>)>,
    ) {
        tokio::spawn(async move {
            let mut buf = vec![0; 1400];
            loop {
                select! {
                    result = socket.recv_from(&mut buf) => {
                        match result {
                            Ok((_bytes_received, _source_address )) => {
                                let parse_result = Message::<PayloadDefinitions>::from_reader(&mut buf.as_slice()).map_err(Error::from);
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
                            Some(message) => {
                                trace!("Sending: {:?}", message);
                                let message_length = message.1.to_writer(&mut buf.as_mut_slice()).unwrap();
                                socket.send_to(&buf[..message_length], message.0).await.unwrap();
                            }
                            None => {
                                info!("Socket Dropping");
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
