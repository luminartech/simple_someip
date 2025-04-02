use std::net::{IpAddr, Ipv4Addr};

use tokio::{net::UdpSocket, select, sync::mpsc};
use tracing::{error, info, trace};

use crate::{
    Error,
    protocol::Message,
    traits::{PayloadWireFormat, WireFormat},
};

#[derive(Debug)]
pub struct SocketManager<PayloadDefinitions> {
    receiver: mpsc::Receiver<Result<Message<PayloadDefinitions>, Error>>,
    sender: mpsc::Sender<Message<PayloadDefinitions>>,
    port: u16,
    session_id: u16,
}

impl<PayloadDefinitions> SocketManager<PayloadDefinitions>
where
    PayloadDefinitions: PayloadWireFormat + 'static,
{
    pub async fn bind_multicast(
        interface: Ipv4Addr,
        multicast_group: Ipv4Addr,
        port: u16,
    ) -> Result<Self, Error> {
        let (rx_tx, rx_rx) = mpsc::channel(16);
        let (tx_tx, tx_rx) = mpsc::channel(16);
        let bind_addr = std::net::SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
        let socket = UdpSocket::bind(bind_addr).await?;
        socket.join_multicast_v4(multicast_group, interface)?;
        Self::spawn_socket_loop(socket, rx_tx, tx_rx);
        Ok(Self {
            receiver: rx_rx,
            sender: tx_tx,
            port,
            session_id: 0,
        })
    }

    pub async fn bind(port: u16) -> Result<Self, Error> {
        let (rx_tx, rx_rx) = mpsc::channel(16);
        let (tx_tx, tx_rx) = mpsc::channel(16);
        let bind_addr = std::net::SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
        let socket = UdpSocket::bind(bind_addr).await?;
        Self::spawn_socket_loop(socket, rx_tx, tx_rx);
        Ok(Self {
            receiver: rx_rx,
            sender: tx_tx,
            port,
            session_id: 0,
        })
    }

    pub async fn receive(&mut self) -> Option<Result<Message<PayloadDefinitions>, Error>> {
        self.receiver.recv().await
    }

    fn spawn_socket_loop(
        socket: UdpSocket,
        rx_tx: mpsc::Sender<Result<Message<PayloadDefinitions>, Error>>,
        mut tx_rx: mpsc::Receiver<Message<PayloadDefinitions>>,
    ) {
        tokio::spawn(async move {
            let mut buf = vec![0; 1400];
            loop {
                select! {
                    result = socket.recv_from(&mut buf) => {
                        match result {
                            Ok((_bytes_received, _source_address )) => {
                                let parse_result = Message::<PayloadDefinitions>::from_reader(&mut buf.as_slice()).map_err(|e| Error::from(e));
                                match rx_tx.send( parse_result ).await {
                                    Ok(_) => {}
                                    Err(_) => {
                                        info!("Discovery Socket Dropping");
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
                                message.to_writer(&mut buf.as_mut_slice()).unwrap();
                                socket.send(&buf).await.unwrap();
                            }
                            None => {
                                info!("Discovery Socket Dropping");
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
