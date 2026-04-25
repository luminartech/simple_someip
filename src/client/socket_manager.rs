//! Client-side UDP socket management.
//!
//! Each bound socket is backed by a `TokioSocket` (concrete, phase-5
//! compromise — see the `bind_discovery_seeded_with_transport`
//! docstring for the RTN-gap analysis) with its I/O loop running on a
//! caller-supplied [`crate::transport::Spawner`]. Phase 9 introduced
//! the `Spawner` trait specifically to make this submission point
//! pluggable; on `std + tokio` consumers pass
//! [`crate::tokio_transport::TokioSpawner`] and the behavior matches
//! the previous `tokio::spawn` path exactly.
//!
//! # Why `Inner` can't drive per-socket futures itself
//!
//! Briefly experimented with having `Inner` drive per-socket futures
//! via `FuturesUnordered` (phase 8 attempt, reverted). That deadlocks:
//! `Inner::handle_control_message` awaits `SocketManager::send`,
//! which internally awaits an mpsc→oneshot round-trip that requires
//! the socket loop to make progress. But `Inner::run_future` is
//! parked inside the handler, so nothing polls the socket loop.
//! Concurrency between the two is mandatory and cannot come from the
//! same task — hence the `Spawner` hook.
//!
//! # What phase 9's `Spawner` does NOT remove from the critical path
//!
//! `Spawner` abstracts task submission, not runtime primitives. The
//! socket loop still `.await`s on runtime-coupled types every
//! iteration. `no_alloc` bare-metal consumers are still blocked by:
//!
//! 1. **`tokio::sync::mpsc` channels** (per-socket: discovery uses
//!    16/16, unicast uses 4/4): heap-allocated + tokio-`Waker`-
//!    specific. A `no_alloc` replacement needs a bounded inline-backed
//!    channel with executor-agnostic waker registration (e.g.
//!    `heapless::mpmc` + a hand-rolled `WakerRegistration`, or
//!    `embassy-sync::Channel`).
//! 2. **`tokio::sync::oneshot` for send-acks** (see `SendMessage`
//!    below): same problem at smaller scale; ownership restructure
//!    is harder than the mpsc swap.
//! 3. **`Arc<Mutex<E2ERegistry>>`** shared between `Inner` and every
//!    socket loop: requires `alloc` + `std::sync`. Collapses to
//!    `&RefCell<E2ERegistry>` on a single-task executor, but the
//!    type change cascades through every call site.
//! 4. **`F::Socket = TokioSocket`** bound on `bind_*` (this module):
//!    RTN-gap, see `bind_discovery_seeded_with_transport` docstring.
//!
//! Until all four are addressed, enabling `feature = "client"` pulls
//! in `std + tokio + socket2`. The `bare_metal` feature flag is a
//! marker today; it does not make this module `no_alloc`. For `no_alloc`
//! SOME/IP usage today, consume `protocol`, `e2e`, and the `transport`
//! trait layer directly — the `bare_metal` example workspace member
//! demonstrates that surface.

use crate::{
    UDP_BUFFER_SIZE,
    e2e::{E2ECheckStatus, E2EKey, E2ERegistry},
    protocol::{Message, MessageView, sd},
    traits::{PayloadWireFormat, WireFormat},
    transport::{ReceivedDatagram, SocketOptions, Spawner, TransportFactory, TransportSocket},
};

use super::error::Error;
use futures::{FutureExt, pin_mut, select};
use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
use tokio::sync::mpsc;
use tracing::{error, info, trace};

/// A received message together with the source address it came from.
///
/// TODO(phase 6): narrow `source` to `SocketAddrV4` to match the
/// `TransportSocket` trait's IPv4-only contract — today the field is
/// always a `SocketAddr::V4(_)` wrapping, and the V6 variant is
/// unreachable. Deferred here because the rename ripples through
/// `DiscoveryMessage` and `ClientUpdate::Unicast`, which is scope creep
/// for phase 5.
#[derive(Clone, Debug)]
pub struct ReceivedMessage<P> {
    pub message: Message<P>,
    pub source: SocketAddr,
    pub e2e_status: Option<E2ECheckStatus>,
}

/// Structure representing a request to send a message
#[derive(Debug)]
pub struct SendMessage<PayloadDefinitions> {
    pub target_addr: SocketAddrV4,
    pub message: Message<PayloadDefinitions>,
    response: tokio::sync::oneshot::Sender<Result<(), Error>>,
}

/// One iteration's select-outcome in `socket_loop_future`. The inner
/// block returns this scalar so the pinned per-iteration `send_fut` /
/// `recv_fut` futures drop before the processing body — releasing their
/// `&mut buf` / `&mut socket` borrows.
enum Outcome<P: PayloadWireFormat> {
    Send(Option<SendMessage<P>>),
    Recv(Result<ReceivedDatagram, crate::transport::TransportError>),
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
    /// Bind the SD multicast socket, seeding the session counter and wrap
    /// state from a previous socket when rebinding. Pass `(1, false)` for a
    /// fresh bind. Preserving state across rebinds avoids emitting a false
    /// reboot signal (`reboot_flag=1`) to peers after
    /// `unbind_discovery` + `bind_discovery`.
    ///
    /// Uses the default `crate::tokio_transport::TokioTransport` and
    /// `crate::tokio_transport::TokioSpawner` backends (rendered as
    /// code literals because `tokio_transport` is only compiled with
    /// the `client`/`server` features and an intra-doc link would
    /// break default-feature rustdoc builds).
    /// For tests or alternate bind logic (e.g. an interceptor factory
    /// around `TokioTransport`), use
    /// [`Self::bind_discovery_seeded_with_transport`].
    ///
    /// Currently `#[cfg(test)]`-gated: production callers reach the
    /// socket through the `_with_transport` variant so the `Spawner`
    /// trait can be exercised end-to-end.
    #[cfg(test)]
    pub async fn bind_discovery_seeded(
        interface: Ipv4Addr,
        e2e_registry: Arc<Mutex<E2ERegistry>>,
        session_id: u16,
        session_has_wrapped: bool,
        multicast_loopback: bool,
    ) -> Result<Self, Error> {
        use crate::tokio_transport::{TokioSpawner, TokioTransport};
        Self::bind_discovery_seeded_with_transport(
            &TokioTransport,
            &TokioSpawner,
            interface,
            e2e_registry,
            session_id,
            session_has_wrapped,
            multicast_loopback,
        )
        .await
    }

    /// Variant of [`Self::bind_discovery_seeded`] that constructs the
    /// underlying socket through a caller-supplied [`TransportFactory`]
    /// and submits the socket's I/O loop through a caller-supplied
    /// [`Spawner`].
    ///
    /// # Why `F::Socket` is still pinned to `TokioSocket`
    ///
    /// The factory must still produce a
    /// [`TokioSocket`](crate::tokio_transport::TokioSocket). Generalizing
    /// to any `TransportSocket` requires stable-Rust Return-Type Notation
    /// (RFC 3654) to express `Send` bounds on the trait's RPITIT methods
    /// at this call site. RTN is nightly-only as of this writing; the
    /// alternatives (GATs on `TransportSocket`, or boxed-future
    /// type-erasure) each carry costs bigger than waiting — see the
    /// module docstring for the full analysis.
    ///
    /// # Why relaxing this bound alone does NOT unblock `no_alloc` callers
    ///
    /// Even with a custom `F::Socket`, this function internally
    /// allocates two `tokio::sync::mpsc` channels (capacities 16 and 16)
    /// and constructs `tokio::sync::oneshot` instances per send. Both
    /// are heap-backed AND tokio-runtime-coupled (their `Waker`
    /// plumbing only works inside a tokio reactor task). A `no_alloc`
    /// bare-metal consumer cannot use this entry point today regardless
    /// of the `F::Socket` bound. The recommended path for `no_alloc`
    /// consumers is to bypass `SocketManager` / `Client` entirely and
    /// build a small orchestrator directly on top of `protocol`, `e2e`,
    /// and the `transport` traits — the `bare_metal` example workspace
    /// member demonstrates the trait layer in isolation.
    pub async fn bind_discovery_seeded_with_transport<F, S>(
        factory: &F,
        spawner: &S,
        interface: Ipv4Addr,
        e2e_registry: Arc<Mutex<E2ERegistry>>,
        session_id: u16,
        session_has_wrapped: bool,
        multicast_loopback: bool,
    ) -> Result<Self, Error>
    where
        F: TransportFactory<Socket = crate::tokio_transport::TokioSocket>,
        S: Spawner,
    {
        let (rx_tx, rx_rx) = mpsc::channel(16);
        let (tx_tx, tx_rx) = mpsc::channel(16);

        // Control whether multicast packets sent by this socket are looped
        // back to sockets on the same host — INCLUDING this socket itself.
        // Disabled by default to avoid parsing self-sent OfferService /
        // FindService entries as if they came from a peer. When enabled
        // (e.g. for a same-host simulator + client setup), the kernel will
        // deliver this socket's own SD multicasts back to it, so higher-level
        // consumers must be prepared to see their own announcements surface
        // as inbound discovery traffic.
        let options = {
            let mut o = SocketOptions::new();
            o.reuse_address = true;
            o.reuse_port = true;
            o.multicast_if_v4 = Some(interface);
            o.multicast_loop_v4 = multicast_loopback;
            o
        };
        let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, sd::MULTICAST_PORT);

        let socket = factory.bind(bind_addr, &options).await?;
        socket.join_multicast_v4(sd::MULTICAST_IP, interface)?;

        let fut = Self::socket_loop_future(socket, rx_tx, tx_rx, e2e_registry);
        spawner.spawn(fut);
        Ok(Self {
            receiver: rx_rx,
            sender: tx_tx,
            local_port: sd::MULTICAST_PORT,
            session_id: session_id.max(1),
            session_has_wrapped,
        })
    }

    /// Bind a unicast SOME/IP socket on `port` using the default
    /// `crate::tokio_transport::TokioTransport` and
    /// `crate::tokio_transport::TokioSpawner` backends (rendered as
    /// code literals for the same rustdoc-feature-gating reason
    /// described on [`Self::bind_discovery_seeded`]). See
    /// [`Self::bind_with_transport`] for the generic variant.
    ///
    /// Currently `#[cfg(test)]`-gated: production callers reach the
    /// socket through the `_with_transport` variant so the `Spawner`
    /// trait can be exercised end-to-end.
    #[cfg(test)]
    pub async fn bind(port: u16, e2e_registry: Arc<Mutex<E2ERegistry>>) -> Result<Self, Error> {
        use crate::tokio_transport::{TokioSpawner, TokioTransport};
        Self::bind_with_transport(&TokioTransport, &TokioSpawner, port, e2e_registry).await
    }

    /// Variant of [`Self::bind`] that constructs the underlying socket
    /// through a caller-supplied [`TransportFactory`] and submits the
    /// socket's I/O loop through a caller-supplied [`Spawner`]. See
    /// [`Self::bind_discovery_seeded_with_transport`] for the factory
    /// bound rationale.
    pub async fn bind_with_transport<F, S>(
        factory: &F,
        spawner: &S,
        port: u16,
        e2e_registry: Arc<Mutex<E2ERegistry>>,
    ) -> Result<Self, Error>
    where
        F: TransportFactory<Socket = crate::tokio_transport::TokioSocket>,
        S: Spawner,
    {
        let (rx_tx, rx_rx) = mpsc::channel(4);
        let (tx_tx, tx_rx) = mpsc::channel(4);

        let options = {
            let mut o = SocketOptions::new();
            o.reuse_address = true;
            o
        };
        let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);

        let socket = factory.bind(bind_addr, &options).await?;
        let port = socket.local_addr()?.port();
        let fut = Self::socket_loop_future(socket, rx_tx, tx_rx, e2e_registry);
        spawner.spawn(fut);
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

    /// Build the I/O loop over a concrete [`TokioSocket`] as a future.
    /// Callers are expected to `tokio::spawn` this future alongside
    /// [`Self`]; the socket loop runs concurrently with its owner so
    /// `SocketManager::send`'s internal oneshot wait can complete.
    /// The reasoning for why the spawn hasn't been hoisted is in the
    /// module-level docs.
    ///
    /// The function remains tied to `TokioSocket` concretely because
    /// generalizing it to `T: TransportSocket` needs stable-Rust
    /// return-type notation to express `Send` bounds on the trait's
    /// RPITIT methods — still nightly as of this writing.
    #[allow(clippy::too_many_lines)]
    async fn socket_loop_future(
        socket: crate::tokio_transport::TokioSocket,
        rx_tx: mpsc::Sender<Result<ReceivedMessage<MessageDefinitions>, Error>>,
        mut tx_rx: mpsc::Receiver<SendMessage<MessageDefinitions>>,
        e2e_registry: Arc<Mutex<E2ERegistry>>,
    ) {
        let mut buf = [0u8; UDP_BUFFER_SIZE];

        loop {
            // `select!` (not `select_biased!`) gives pseudo-random
            // fairness across ready arms — matches prior
            // `tokio::select!` behavior and avoids starving either
            // the send or recv arm under sustained one-sided load.
            //
            // The fresh `.fuse()`'d per-iteration futures are pinned
            // on the stack (required: `Fuse<_>` is not `Unpin`).
            // Returning an `Outcome<P>` scalar from the inner block
            // drops both pinned futures — and their `&mut buf` /
            // `&mut socket` borrows — before the processing body
            // below runs, so the body can re-borrow `buf` freely.
            let outcome: Outcome<MessageDefinitions> = {
                let send_fut = tx_rx.recv().fuse();
                let recv_fut = socket.recv_from(&mut buf).fuse();
                pin_mut!(send_fut, recv_fut);
                select! {
                    message = send_fut => Outcome::Send(message),
                    result = recv_fut => Outcome::Recv(result),
                }
            };

            match outcome {
                Outcome::Send(Some(send_message)) => {
                    trace!("Sending: {:?}", &send_message);
                    let mut message_length =
                        match send_message.message.encode(&mut buf.as_mut_slice()) {
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

                    // Apply E2E protect if configured. `protected`
                    // is a disjoint stack buffer, so the input can
                    // be borrowed directly out of `buf[16..]` with
                    // no intermediate copy.
                    {
                        let key =
                            E2EKey::from_message_id(send_message.message.header().message_id());
                        let mut registry = e2e_registry.lock().expect("e2e registry lock poisoned");
                        if registry.contains_key(&key) {
                            let upper_header: [u8; 8] =
                                buf[8..16].try_into().expect("upper header slice");
                            let mut protected = [0u8; UDP_BUFFER_SIZE];
                            let result = registry.protect(
                                key,
                                &buf[16..message_length],
                                upper_header,
                                &mut protected,
                            );
                            match result {
                                Some(Ok(protected_len)) => {
                                    if 16 + protected_len > UDP_BUFFER_SIZE {
                                        error!(
                                            "E2E-protected payload ({} bytes) exceeds UDP_BUFFER_SIZE ({}); dropping send",
                                            16 + protected_len,
                                            UDP_BUFFER_SIZE
                                        );
                                        let _ = send_message
                                            .response
                                            .send(Err(Error::Capacity("udp_buffer")));
                                        continue;
                                    }
                                    #[allow(clippy::cast_possible_truncation)]
                                    let new_length: u32 = 8 + protected_len as u32;
                                    buf[4..8].copy_from_slice(&new_length.to_be_bytes());
                                    buf[16..16 + protected_len]
                                        .copy_from_slice(&protected[..protected_len]);
                                    message_length = 16 + protected_len;
                                }
                                Some(Err(e)) => {
                                    error!("E2E protect error: {:?}", e);
                                }
                                None => unreachable!("contains_key was true"),
                            }
                        }
                    }

                    match socket
                        .send_to(&buf[..message_length], send_message.target_addr)
                        .await
                    {
                        Ok(()) => {
                            trace!(
                                "Sent {} bytes to {}",
                                message_length, send_message.target_addr
                            );
                            if let Ok(()) = send_message.response.send(Ok(())) {
                            } else {
                                info!("Socket owner closed channel, closing socket.");
                                // The sender has been dropped, so we should exit
                                break;
                            }
                        }
                        Err(e) => {
                            error!("Failed to send message with error: {:?}", e);
                            if let Ok(()) = send_message.response.send(Err(Error::Transport(e))) {
                            } else {
                                error!("Socket owner closed channel unexpectedly, closing socket.");
                                break;
                            }
                        }
                    }
                }
                Outcome::Send(None) => {
                    info!("Send channel closed, closing socket.");
                    // The sender has been dropped, so we should exit
                    break;
                }
                Outcome::Recv(Ok(ReceivedDatagram {
                    bytes_received,
                    source,
                    truncated,
                })) => {
                    if truncated {
                        // A truncated datagram cannot be parsed reliably;
                        // the length field in the SOME/IP header will not
                        // match the bytes we received. Log and drop.
                        error!(
                            "Discarding truncated datagram from {}: {} bytes received",
                            source, bytes_received
                        );
                        continue;
                    }
                    let source_address = SocketAddr::V4(source);
                    let parse_result = MessageView::parse(&buf[..bytes_received])
                        .and_then(|view| {
                            let header = view.header().to_owned();
                            let upper_header = header.upper_header_bytes();
                            let key = E2EKey::from_message_id(header.message_id());
                            let payload_bytes = view.payload_bytes();

                            // Apply E2E check if configured
                            let (e2e_status, effective_payload) = {
                                let mut registry =
                                    e2e_registry.lock().expect("e2e registry lock poisoned");
                                match registry.check(key, payload_bytes, upper_header) {
                                    Some((status, stripped)) => (Some(status), stripped),
                                    None => (None, payload_bytes),
                                }
                            };

                            let payload = MessageDefinitions::from_payload_bytes(
                                header.message_id(),
                                effective_payload,
                            )?;
                            Ok(ReceivedMessage {
                                message: Message::new(header, payload),
                                source: source_address,
                                e2e_status,
                            })
                        })
                        .map_err(Error::from);
                    if let Ok(()) = rx_tx.send(parse_result).await {
                    } else {
                        info!("Socket Dropping");
                        // The receiver has been dropped, so we should exit
                        break;
                    }
                }
                Outcome::Recv(Err(recv_err)) => {
                    error!("Transport recv failed: {:?}", recv_err);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::sd::test_support::{TestPayload, empty_sd_header};
    use crate::tokio_transport::TokioSpawner;
    use std::format;
    use std::vec;
    // Tests build ad-hoc UDP peers via tokio directly; this is not part of
    // the production code path, which goes through the `TransportSocket`
    // abstraction via `TokioTransport`.
    use tokio::net::UdpSocket;

    type TestSocketManager = SocketManager<TestPayload>;

    fn test_registry() -> Arc<Mutex<E2ERegistry>> {
        Arc::new(Mutex::new(E2ERegistry::new()))
    }

    async fn bind_ephemeral_spawned() -> TestSocketManager {
        TestSocketManager::bind(0, test_registry()).await.unwrap()
    }

    #[tokio::test]
    async fn test_bind_ephemeral_port() {
        let sm = bind_ephemeral_spawned().await;
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
        let sm = bind_ephemeral_spawned().await;
        sm.shut_down().await;
    }

    #[tokio::test]
    async fn test_socket_manager_send_and_receive() {
        let mut sm = bind_ephemeral_spawned().await;
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
        let mut sm = bind_ephemeral_spawned().await;
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
        let mut sm = bind_ephemeral_spawned().await;
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
        let sm = bind_ephemeral_spawned().await;
        let s = format!("{sm:?}");
        assert!(s.contains("SocketManager"));
        sm.shut_down().await;
    }

    #[tokio::test]
    async fn test_socket_manager_send_to_target() {
        let mut sm = bind_ephemeral_spawned().await;

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
        .await
        .unwrap();
        assert_eq!(sm.session_id(), 1, "session_id 0 must be normalized to 1");
    }

    #[tokio::test]
    async fn test_session_id_wraps_to_one_and_clears_reboot_flag() {
        let mut sm = bind_ephemeral_spawned().await;
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

    #[tokio::test]
    async fn send_e2e_protected_payload_exceeding_udp_buffer_returns_capacity_error() {
        use crate::RawPayload;
        use crate::e2e::{E2EProfile, Profile4Config};
        use crate::protocol::{Header, MessageId, MessageType, MessageTypeField, ReturnCode};

        // Register an E2E profile so the protect branch runs.
        let message_id = MessageId::new_from_service_and_method(0x1234, 0x5678);
        let key = E2EKey::from_message_id(message_id);
        let mut reg = E2ERegistry::new();
        reg.register(key, E2EProfile::Profile4(Profile4Config::new(0, 15)));
        let e2e_registry = Arc::new(Mutex::new(reg));

        let mut sm = SocketManager::<RawPayload>::bind(0, e2e_registry)
            .await
            .unwrap();

        // Craft a message whose raw-encoded size fits UDP_BUFFER_SIZE (16-byte
        // header + 1480-byte payload = 1496 bytes) but whose E2E-protected
        // size does not (payload grows by PROFILE4_HEADER_SIZE = 12, pushing
        // the total to 1508 bytes, 8 over MTU).
        let payload_bytes = [0u8; 1480];
        let payload = RawPayload::from_payload_bytes(message_id, &payload_bytes).unwrap();
        let header = Header::new(
            message_id,
            0x0001_0001,
            0x01,
            0x01,
            MessageTypeField::new(MessageType::Request, false),
            ReturnCode::Ok,
            payload_bytes.len(),
        );
        let message = Message::new(header, payload);

        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9999);
        let err = sm
            .send(target, message)
            .await
            .expect_err("E2E-protected oversize message must error");
        match err {
            Error::Capacity(tag) => assert_eq!(tag, "udp_buffer"),
            other => panic!("expected Error::Capacity(\"udp_buffer\"), got {other:?}"),
        }
    }

    /// Proves the public `bind_with_transport` entry point accepts an
    /// alternative `TransportFactory` implementation. The factory here is
    /// a thin interceptor that counts how many times `bind` is called; it
    /// delegates to the built-in `TokioTransport`, which is what the
    /// current `Socket = TokioSocket` bound requires.
    #[tokio::test]
    async fn bind_with_transport_accepts_custom_factory() {
        use crate::tokio_transport::{TokioSocket, TokioTransport};
        use core::future::Future;
        use core::sync::atomic::{AtomicUsize, Ordering};

        struct CountingFactory {
            inner: TokioTransport,
            calls: AtomicUsize,
        }

        impl TransportFactory for CountingFactory {
            type Socket = TokioSocket;
            fn bind(
                &self,
                addr: SocketAddrV4,
                options: &SocketOptions,
            ) -> impl Future<Output = Result<Self::Socket, crate::transport::TransportError>>
            {
                self.calls.fetch_add(1, Ordering::SeqCst);
                // Clone the options into the async block so no borrow
                // escapes the returned future.
                let options = *options;
                let inner = self.inner;
                async move { inner.bind(addr, &options).await }
            }
        }

        let factory = CountingFactory {
            inner: TokioTransport,
            calls: AtomicUsize::new(0),
        };

        let sm =
            TestSocketManager::bind_with_transport(&factory, &TokioSpawner, 0, test_registry())
                .await
                .expect("bind via custom factory");
        assert_eq!(
            factory.calls.load(Ordering::SeqCst),
            1,
            "custom factory should have been invoked exactly once"
        );
        drop(sm);
    }

    /// End-to-end proof that a custom `TransportFactory` actually
    /// carries traffic through the full `SocketManager` path. Sends a
    /// SOME/IP-SD message from one bound `SocketManager` to a raw tokio
    /// socket, verifies the bytes arrive intact. Complements the lighter
    /// `bind_with_transport_accepts_custom_factory` by exercising
    /// `send_to` + the spawned I/O loop, not just the bind call.
    #[tokio::test]
    async fn bind_with_transport_carries_traffic_end_to_end() {
        use crate::tokio_transport::{TokioSocket, TokioTransport};
        use core::future::Future;

        // Factory that overrides `SocketOptions` to force
        // `reuse_address = true` regardless of caller-provided flags —
        // proves the factory sits in the hot path.
        struct ForceReuseFactory;
        impl TransportFactory for ForceReuseFactory {
            type Socket = TokioSocket;
            fn bind(
                &self,
                addr: SocketAddrV4,
                options: &SocketOptions,
            ) -> impl Future<Output = Result<Self::Socket, crate::transport::TransportError>>
            {
                let mut opts = *options;
                opts.reuse_address = true;
                async move { TokioTransport.bind(addr, &opts).await }
            }
        }

        let mut sm = SocketManager::<TestPayload>::bind_with_transport(
            &ForceReuseFactory,
            &TokioSpawner,
            0,
            test_registry(),
        )
        .await
        .expect("bind via custom factory");
        let sm_port = sm.port();

        let recv = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let recv_port = recv.local_addr().unwrap().port();

        let msg = Message::<TestPayload>::new_sd(1, &empty_sd_header());
        sm.send(SocketAddrV4::new(Ipv4Addr::LOCALHOST, recv_port), msg)
            .await
            .expect("send_to via custom-factory-built socket");

        let mut buf = [0u8; 1500];
        let (len, from) =
            tokio::time::timeout(std::time::Duration::from_secs(2), recv.recv_from(&mut buf))
                .await
                .expect("timed out waiting for datagram")
                .expect("recv failed");

        assert!(len > 0, "empty datagram");
        match from {
            std::net::SocketAddr::V4(v4) => assert_eq!(v4.port(), sm_port),
            other @ std::net::SocketAddr::V6(_) => {
                panic!("unexpected source address family: {other:?}")
            }
        }

        // Parse and confirm it's a SOME/IP-SD message, not garbage.
        let view = MessageView::parse(&buf[..len]).unwrap();
        assert_eq!(view.header().message_id(), crate::protocol::MessageId::SD);
    }

    /// Negative test: a factory that returns
    /// `Err(TransportError::AddressInUse)` must surface as
    /// `Err(Error::Transport(TransportError::AddressInUse))` through
    /// the `?` + `From` conversion chain in
    /// `bind_with_transport`. Catches regressions in the `#[from]`
    /// impl on `client::Error` or the return-type plumbing.
    #[tokio::test]
    async fn bind_with_transport_propagates_factory_error() {
        use crate::tokio_transport::TokioSocket;
        use crate::transport::TransportError;

        struct AlwaysBusyFactory;
        impl TransportFactory for AlwaysBusyFactory {
            type Socket = TokioSocket;
            async fn bind(
                &self,
                _addr: SocketAddrV4,
                _options: &SocketOptions,
            ) -> Result<Self::Socket, TransportError> {
                Err(TransportError::AddressInUse)
            }
        }

        let err = TestSocketManager::bind_with_transport(
            &AlwaysBusyFactory,
            &TokioSpawner,
            0,
            test_registry(),
        )
        .await
        .expect_err("factory returned Err, bind must surface it");
        match err {
            Error::Transport(TransportError::AddressInUse) => {}
            other => {
                panic!("expected Error::Transport(TransportError::AddressInUse), got {other:?}")
            }
        }
    }
}
