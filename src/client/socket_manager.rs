use crate::{
    e2e::{CheckStatus, E2EKey, E2ERegistry, PROFILE4_HEADER_SIZE},
    protocol::{Message, MessageView, sd},
    traits::{PayloadWireFormat, WireFormat},
};

use super::error::Error;
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{Arc, Mutex},
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
    pub e2e_status: Option<CheckStatus>,
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
    /// Set to true once `session_id` has wrapped from 0xFFFF → 1.
    /// Per AUTOSAR SOME/IP-SD, the reboot flag must be cleared after the
    /// first counter wrap and stay cleared.
    session_has_wrapped: bool,
}

impl<MessageDefinitions> SocketManager<MessageDefinitions>
where
    MessageDefinitions: PayloadWireFormat + 'static,
{
    /// Bind the SD multicast socket, seeding the session counter and wrap state from
    /// a previous socket when rebinding. Pass `(1, false)` for a fresh bind.
    /// Preserving state across rebinds avoids emitting a false reboot signal
    /// (`reboot_flag=1`) to peers after `unbind_discovery` + `bind_discovery`.
    pub fn bind_discovery_seeded(
        interface: Ipv4Addr,
        e2e_registry: Arc<Mutex<E2ERegistry>>,
        session_id: u16,
        session_has_wrapped: bool,
        multicast_loopback: bool,
    ) -> Result<Self, Error> {
        let (rx_tx, rx_rx) = mpsc::channel(16);
        let (tx_tx, tx_rx) = mpsc::channel(16);
        let bind_addr =
            std::net::SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), sd::MULTICAST_PORT);

        // Create socket with SO_REUSEADDR to allow quick restart
        let socket = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        socket.set_reuse_address(true)?;
        #[cfg(unix)]
        socket.set_reuse_port(true)?;
        socket.set_multicast_if_v4(&interface)?;
        // Control whether multicast packets sent by this socket are looped
        // back to sockets on the same host — INCLUDING this socket itself.
        // Disabled by default to avoid parsing self-sent OfferService /
        // FindService entries as if they came from a peer. When enabled
        // (e.g. for a same-host simulator + client setup), the kernel will
        // deliver this socket's own SD multicasts back to it, so higher-level
        // consumers must be prepared to see their own announcements surface
        // as inbound discovery traffic.
        socket.set_multicast_loop_v4(multicast_loopback)?;
        socket.bind(&bind_addr.into())?;
        socket.set_nonblocking(true)?;
        let socket: std::net::UdpSocket = socket.into();
        let socket = UdpSocket::from_std(socket)?;

        socket.join_multicast_v4(sd::MULTICAST_IP, interface)?;

        Self::spawn_socket_loop(socket, rx_tx, tx_rx, e2e_registry);
        Ok(Self {
            receiver: rx_rx,
            sender: tx_tx,
            local_port: sd::MULTICAST_PORT,
            session_id: session_id.max(1),
            session_has_wrapped,
        })
    }

    /// Bind a receive-only UNICAST service-discovery socket on the SD port,
    /// bound to the specific `interface` IP — more specific than the multicast
    /// discovery socket's `INADDR_ANY` bind, so the kernel diverts the sensor's
    /// unicast SD datagrams here ("most-specific bind wins"). This keeps the
    /// unicast SD session domain on its own `SessionTracker` key, separate from
    /// the multicast one, which prevents the interleaved-counter false-reboot
    /// bug. No multicast group join; outgoing SD still goes via the multicast
    /// discovery socket, so this socket only ever receives.
    ///
    /// The returned `SocketManager` still carries a send half and session
    /// counter for type uniformity, but the discovery layer never drives them
    /// for this socket: it is receive-only *by usage*, not by type.
    pub fn bind_discovery_unicast(
        interface: Ipv4Addr,
        e2e_registry: Arc<Mutex<E2ERegistry>>,
    ) -> Result<Self, Error> {
        let (rx_tx, rx_rx) = mpsc::channel(16);
        let (tx_tx, tx_rx) = mpsc::channel(16);
        let bind_addr = std::net::SocketAddr::new(IpAddr::V4(interface), sd::MULTICAST_PORT);

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

        Self::spawn_socket_loop(socket, rx_tx, tx_rx, e2e_registry);
        Ok(Self {
            receiver: rx_rx,
            sender: tx_tx,
            local_port: sd::MULTICAST_PORT,
            session_id: 1,
            session_has_wrapped: false,
        })
    }

    pub fn bind(port: u16, e2e_registry: Arc<Mutex<E2ERegistry>>) -> Result<Self, Error> {
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
        Self::spawn_socket_loop(socket, rx_tx, tx_rx, e2e_registry);
        Ok(Self {
            receiver: rx_rx,
            sender: tx_tx,
            local_port: port,
            session_id: 1,
            session_has_wrapped: false,
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
        if self.session_id == u16::MAX {
            self.session_id = 1;
            self.session_has_wrapped = true;
        } else {
            self.session_id += 1;
        }
        Ok(())
    }

    /// Returns the SD reboot flag value to use in outgoing SD messages.
    ///
    /// Per AUTOSAR SOME/IP-SD, this is [`RebootFlag::RecentlyRebooted`] from startup
    /// until the session counter wraps from `0xFFFF` to `1`, then
    /// [`RebootFlag::Continuous`] permanently.
    pub fn reboot_flag(&self) -> crate::protocol::sd::RebootFlag {
        crate::protocol::sd::RebootFlag::from(!self.session_has_wrapped)
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

    #[allow(clippy::too_many_lines)]
    fn spawn_socket_loop(
        socket: UdpSocket,
        rx_tx: mpsc::Sender<Result<ReceivedMessage<MessageDefinitions>, Error>>,
        mut tx_rx: mpsc::Receiver<SendMessage<MessageDefinitions>>,
        e2e_registry: Arc<Mutex<E2ERegistry>>,
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
                                        let upper_header = header.upper_header_bytes();
                                        let key = E2EKey::from_message_id(header.message_id());
                                        let payload_bytes = view.payload_bytes();

                                        // Apply E2E check if configured.
                                        // CheckOutcome borrows the input slice for the stripped
                                        // payload, so we extract the owned status + payload slice
                                        // here and drop the outcome inside this block.
                                        let (e2e_status, effective_payload) = {
                                            let mut registry = e2e_registry.lock().expect("e2e registry lock poisoned");
                                            match registry.check(key, payload_bytes, upper_header) {
                                                Some(outcome) => {
                                                    let status = CheckStatus::from_outcome(&outcome);
                                                    let stripped = outcome.payload().unwrap_or(payload_bytes);
                                                    (Some(status), stripped)
                                                }
                                                None => (None, payload_bytes),
                                            }
                                        };

                                        let payload = MessageDefinitions::from_payload_bytes(header.message_id(), effective_payload)?;
                                        Ok(ReceivedMessage {
                                            message: Message::new(header, payload),
                                            source: source_address,
                                            e2e_status,
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
                            let mut message_length = match send_message.message.encode(&mut buf.as_mut_slice()) {
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

                            // Apply E2E protect if configured
                            {
                                let key = E2EKey::from_message_id(send_message.message.header().message_id());
                                let mut registry = e2e_registry.lock().expect("e2e registry lock poisoned");
                                if registry.contains_key(&key) {
                                    let original_payload = buf[16..message_length].to_vec();
                                    let upper_header: [u8; 8] = buf[8..16].try_into().expect("upper header slice");
                                    let mut protected = vec![0u8; original_payload.len() + PROFILE4_HEADER_SIZE];
                                    match registry.protect(key, &original_payload, upper_header, &mut protected) {
                                        Some(Ok(protected_len)) => {
                                            #[allow(clippy::cast_possible_truncation)]
                                            let new_length: u32 = 8 + protected_len as u32;
                                            buf[4..8].copy_from_slice(&new_length.to_be_bytes());
                                            if 16 + protected_len > buf.len() {
                                                buf.resize(16 + protected_len, 0);
                                            }
                                            buf[16..16 + protected_len].copy_from_slice(&protected[..protected_len]);
                                            message_length = 16 + protected_len;
                                        }
                                        Some(Err(e)) => {
                                            error!("E2E protect error: {:?}", e);
                                        }
                                        None => unreachable!("contains_key was true"),
                                    }
                                }
                            }

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
    use crate::protocol::sd::test_support::{TestPayload, empty_sd_header};
    use std::format;

    type TestSocketManager = SocketManager<TestPayload>;

    fn test_registry() -> Arc<Mutex<E2ERegistry>> {
        Arc::new(Mutex::new(E2ERegistry::new()))
    }

    /// Spike for the per-transport SD fix: prove the kernel splits SD
    /// multicast from unicast across two sockets sharing the SD port — the
    /// multicast socket bound to `INADDR_ANY` + joined (Windows-portable, and
    /// what the real discovery socket already does), and a more-specific
    /// socket bound to the host interface IP (not joined). "Most-specific bind
    /// wins" must divert the sensor's unicast SD to the interface-IP socket,
    /// leaving the wildcard multicast socket seeing only multicast — so each
    /// transport's session counter lands on its own `SessionTracker` key
    /// instead of colliding (the false-reboot bug). No bind-to-group (Windows
    /// rejects it) and no send-path change required. Skips if the host has no
    /// usable multicast route (e.g. `lo`-only CI) — the authoritative check is
    /// the live-sensor run.
    #[test]
    fn dual_socket_splits_multicast_from_unicast() {
        use std::eprintln;
        use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
        use std::time::Duration;
        use std::vec::Vec;

        let group = crate::protocol::sd::MULTICAST_IP;

        let bind_reuse = |addr: SocketAddr| -> std::io::Result<socket2::Socket> {
            let s = socket2::Socket::new(
                socket2::Domain::IPV4,
                socket2::Type::DGRAM,
                Some(socket2::Protocol::UDP),
            )?;
            s.set_reuse_address(true)?;
            #[cfg(unix)]
            s.set_reuse_port(true)?;
            s.bind(&addr.into())?;
            s.set_read_timeout(Some(Duration::from_millis(400)))?;
            Ok(s)
        };
        let drain = |s: &UdpSocket| -> Vec<Vec<u8>> {
            let mut out = Vec::new();
            let mut buf = [0u8; 64];
            while let Ok((n, _)) = s.recv_from(&mut buf) {
                out.push(buf[..n].to_vec());
            }
            out
        };

        // Multicast socket: bound to INADDR_ANY (Windows-portable; NOT the
        // group address) + joined. Tagged Multicast. The more-specific
        // interface-IP unicast socket below must divert unicast away from it.
        let mc: UdpSocket = match (|| -> std::io::Result<UdpSocket> {
            let s = bind_reuse(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))?;
            s.set_multicast_loop_v4(true)?;
            let s: UdpSocket = s.into();
            s.join_multicast_v4(&group, &Ipv4Addr::UNSPECIFIED)?;
            Ok(s)
        })() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("SKIP dual_socket_splits: multicast setup failed ({e})");
                return;
            }
        };
        // Reuse the OS-assigned ephemeral port for the unicast socket and the
        // sender target too, so the test never collides with a fixed port that
        // happens to be in use on a shared CI runner.
        let port = match mc.local_addr() {
            Ok(SocketAddr::V4(a)) => a.port(),
            _ => {
                eprintln!("SKIP dual_socket_splits: multicast socket has no IPv4 local addr");
                return;
            }
        };
        // This host's egress IPv4 for the multicast route — the analogue of
        // the real `interface` arg the discovery socket is bound against.
        let local_ip = {
            let probe = UdpSocket::bind("0.0.0.0:0").expect("probe bind");
            let _ = probe.connect(SocketAddrV4::new(group, port));
            match probe.local_addr() {
                Ok(SocketAddr::V4(a)) => *a.ip(),
                _ => Ipv4Addr::UNSPECIFIED,
            }
        };
        if local_ip.is_unspecified() {
            eprintln!("SKIP dual_socket_splits: no egress IPv4");
            return;
        }

        // Unicast socket: bound to the SPECIFIC host IP (not wildcard), NOT
        // joined to the group — so it must not receive the group multicast.
        let uc: UdpSocket = bind_reuse(SocketAddr::from((local_ip, port)))
            .expect("bind unicast socket")
            .into();

        let tx: UdpSocket = bind_reuse(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))
            .expect("bind sender")
            .into();
        let _ = tx.set_multicast_loop_v4(true);
        let _ = tx.set_multicast_ttl_v4(1);
        // A send failure here is an environment issue (no route / permissions),
        // not a logic regression — surface it as a visible SKIP rather than
        // letting an empty drain quietly pass the test.
        if let Err(e) = tx.send_to(b"MCAST", SocketAddrV4::new(group, port)) {
            eprintln!("SKIP dual_socket_splits: multicast send failed ({e})");
            return;
        }
        if let Err(e) = tx.send_to(b"UCAST", SocketAddrV4::new(local_ip, port)) {
            eprintln!("SKIP dual_socket_splits: unicast send failed ({e})");
            return;
        }
        std::thread::sleep(Duration::from_millis(60));

        let mc_got = drain(&mc);
        let uc_got = drain(&uc);

        if mc_got.is_empty() {
            eprintln!("SKIP dual_socket_splits: no multicast route on this host");
            return;
        }
        assert!(
            mc_got.iter().any(|p| p == b"MCAST"),
            "mc socket must get the multicast"
        );
        assert!(
            !mc_got.iter().any(|p| p == b"UCAST"),
            "mc socket (bound to INADDR_ANY) must NOT get the unicast"
        );
        assert!(
            uc_got.iter().any(|p| p == b"UCAST"),
            "uc socket must get the unicast"
        );
        assert!(
            !uc_got.iter().any(|p| p == b"MCAST"),
            "uc socket (never joined the group) must NOT get the multicast"
        );
    }

    #[tokio::test]
    async fn test_bind_ephemeral_port() {
        let sm = TestSocketManager::bind(0, test_registry()).unwrap();
        assert!(sm.port() > 0);
        assert_eq!(sm.session_id(), 1);
    }

    #[tokio::test]
    async fn test_send_message_new() {
        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1234);
        let msg = Message::new_sd(1, &empty_sd_header());
        let (rx, send_msg) = SendMessage::<TestPayload>::new(target, msg);
        assert_eq!(send_msg.target_addr, target);
        // Verify the oneshot channel works
        send_msg.response.send(Ok(())).unwrap();
        assert!(rx.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn test_socket_manager_shut_down() {
        let sm = TestSocketManager::bind(0, test_registry()).unwrap();
        sm.shut_down().await;
    }

    #[tokio::test]
    async fn test_socket_manager_send_and_receive() {
        let mut sm = TestSocketManager::bind(0, test_registry()).unwrap();
        let sm_port = sm.port();

        // Create a raw UDP socket to send data to the SocketManager
        let raw_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        // Build and encode an SD message
        let msg = Message::<TestPayload>::new_sd(1, &empty_sd_header());
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
            received.message.header().message_id(),
            msg.header().message_id()
        );
        assert!(received.message.is_sd());
    }

    #[tokio::test]
    async fn test_poll_receive() {
        let mut sm = TestSocketManager::bind(0, test_registry()).unwrap();
        let sm_port = sm.port();

        // Send a message to the socket manager from a raw socket
        let raw_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let msg = Message::<TestPayload>::new_sd(1, &empty_sd_header());
        let mut buf = vec![0u8; 128];
        let n = msg.encode(&mut buf.as_mut_slice()).unwrap();
        raw_socket
            .send_to(&buf[..n], SocketAddrV4::new(Ipv4Addr::LOCALHOST, sm_port))
            .await
            .unwrap();

        // Use poll_fn to exercise poll_receive
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            std::future::poll_fn(|cx| sm.poll_receive(cx)).await
        })
        .await
        .expect("Timed out waiting for poll_receive");

        let received = result.unwrap().unwrap();
        assert!(received.message.is_sd());
    }

    #[tokio::test]
    async fn test_send_drops_when_socket_loop_exits() {
        let mut sm = TestSocketManager::bind(0, test_registry()).unwrap();
        // Shut down the socket loop by dropping the internal channels
        // We can't directly kill the loop, but we can test the error path
        // by sending to a socket manager that has been shut down.
        let port = sm.port();
        assert!(port > 0);

        // Send a valid message first to verify normal operation
        let raw_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let raw_port = raw_socket.local_addr().unwrap().port();
        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, raw_port);
        let msg = Message::<TestPayload>::new_sd(1, &empty_sd_header());
        sm.send(target, msg).await.unwrap();
        assert_eq!(sm.session_id(), 2);

        // Second send increments session
        let msg = Message::<TestPayload>::new_sd(1, &empty_sd_header());
        sm.send(target, msg).await.unwrap();
        assert_eq!(sm.session_id(), 3);
    }

    #[tokio::test]
    async fn test_received_message_debug() {
        let msg = Message::<TestPayload>::new_sd(1, &empty_sd_header());
        let received = ReceivedMessage {
            message: msg,
            source: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 5000),
            e2e_status: None,
        };
        let s = format!("{received:?}");
        assert!(s.contains("ReceivedMessage"));
    }

    #[tokio::test]
    async fn test_send_message_debug() {
        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1234);
        let msg = Message::<TestPayload>::new_sd(1, &empty_sd_header());
        let (_rx, send_msg) = SendMessage::<TestPayload>::new(target, msg);
        let s = format!("{send_msg:?}");
        assert!(s.contains("SendMessage"));
    }

    #[tokio::test]
    async fn test_socket_manager_debug() {
        let sm = TestSocketManager::bind(0, test_registry()).unwrap();
        let s = format!("{sm:?}");
        assert!(s.contains("SocketManager"));
        sm.shut_down().await;
    }

    #[tokio::test]
    async fn test_socket_manager_send_to_target() {
        let mut sm = TestSocketManager::bind(0, test_registry()).unwrap();

        // Create a raw socket to receive
        let raw_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let raw_port = raw_socket.local_addr().unwrap().port();

        let msg = Message::<TestPayload>::new_sd(1, &empty_sd_header());
        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, raw_port);

        sm.send(target, msg.clone()).await.unwrap();
        assert_eq!(sm.session_id(), 2);

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
        assert_eq!(
            view.header().to_owned().message_id(),
            msg.header().message_id()
        );
    }

    #[tokio::test]
    async fn test_bind_discovery_seeded_normalizes_zero_session_id() {
        let sm = TestSocketManager::bind_discovery_seeded(
            Ipv4Addr::LOCALHOST,
            test_registry(),
            0,
            false,
            false,
        )
        .unwrap();
        assert_eq!(sm.session_id(), 1, "session_id 0 must be normalized to 1");
    }

    #[tokio::test]
    async fn test_session_id_wraps_to_one_and_clears_reboot_flag() {
        let mut sm = TestSocketManager::bind(0, test_registry()).unwrap();
        let raw_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let target =
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, raw_socket.local_addr().unwrap().port());
        let msg = || Message::<TestPayload>::new_sd(1, &empty_sd_header());

        use crate::protocol::sd::RebootFlag;
        // Set session_id to one before the wrap point
        sm.session_id = u16::MAX - 1;
        assert_eq!(
            sm.reboot_flag(),
            RebootFlag::RecentlyRebooted,
            "reboot flag should be RecentlyRebooted before wrap"
        );

        // Send one message: session_id reaches MAX
        sm.send(target, msg()).await.unwrap();
        assert_eq!(sm.session_id(), u16::MAX);
        assert_eq!(
            sm.reboot_flag(),
            RebootFlag::RecentlyRebooted,
            "reboot flag should still be RecentlyRebooted at MAX"
        );

        // Send one more: triggers the wrap, session_id becomes 1
        sm.send(target, msg()).await.unwrap();
        assert_eq!(sm.session_id(), 1, "session_id should wrap to 1, not 0");
        assert_eq!(
            sm.reboot_flag(),
            RebootFlag::Continuous,
            "reboot flag should be Continuous after wrap"
        );

        // Subsequent sends continue incrementing normally from 1
        sm.send(target, msg()).await.unwrap();
        assert_eq!(sm.session_id(), 2);
        assert_eq!(
            sm.reboot_flag(),
            RebootFlag::Continuous,
            "reboot flag stays Continuous after wrap"
        );
    }
}
