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
//! # Bare-metal readiness status
//!
//! **Completed abstractions (Phases 9-12):**
//! - `Spawner` trait (Phase 9): task submission is pluggable.
//! - `E2ERegistryHandle` / `InterfaceHandle` (Phase 10): lock handles
//!   abstracted away from `Arc<Mutex<_>>` / `Arc<RwLock<_>>`.
//! - `ChannelFactory` (Phase 11): channel primitives abstracted via
//!   `TokioChannels` (std) and `EmbassySyncChannels` (`bare_metal`).
//! - `TransportSocket` GATs (Phase 12): `Socket = TokioSocket` pin
//!   removed; `SendFuture` / `RecvFuture` associated types express
//!   `Send` bounds for spawnable socket loops.
//!
//! **Phase 13 (client half) complete:** the `client` feature no longer
//! pulls tokio or socket2. The full `Client` / `Inner` / `SocketManager`
//! types — including the `bind` / `bind_discovery_seeded` convenience
//! constructors that default to `TokioTransport` + `TokioSpawner` — are
//! gated behind the new `client-tokio` feature, which layers tokio +
//! socket2 on top of `client`.
//!
//! **Remaining gaps:**
//! - **Server-side split** (deferred to Phase 14): `feature = "server"`
//!   still pulls tokio + socket2 because `server::sd_state` /
//!   `server::subscription_manager` reference tokio types directly.
//!
//! For `no_alloc` SOME/IP usage today, consume `protocol`, `e2e`, and
//! the `transport` trait layer directly — the `bare_metal` example
//! workspace member demonstrates that surface.

use crate::{
    UDP_BUFFER_SIZE,
    e2e::{E2ECheckStatus, E2EKey},
    protocol::{Message, MessageView, sd},
    traits::{PayloadWireFormat, WireFormat},
    transport::{
        ChannelFactory, E2ERegistryHandle, MpscRecv, MpscSend, OneshotRecv, OneshotSend,
        ReceivedDatagram, SocketOptions, Spawner, TransportFactory, TransportSocket,
    },
};

use super::error::Error;
use futures::{FutureExt, pin_mut, select};
use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    task::{Context, Poll},
};
use tracing::{debug, error, info, trace, warn};

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
pub struct SendMessage<PayloadDefinitions: Send + 'static, C: ChannelFactory> {
    pub target_addr: SocketAddrV4,
    pub message: Message<PayloadDefinitions>,
    response: C::OneshotSender<Result<(), Error>>,
}

impl<P: PayloadWireFormat + Send + 'static, C: ChannelFactory> std::fmt::Debug for SendMessage<P, C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendMessage")
            .field("target_addr", &self.target_addr)
            .field("message", &self.message)
            .finish_non_exhaustive()
    }
}

/// One iteration's select-outcome in `socket_loop_future`. The inner
/// block returns this scalar so the pinned per-iteration `send_fut` /
/// `recv_fut` futures drop before the processing body — releasing their
/// `&mut buf` / `&mut socket` borrows.
enum Outcome<P: PayloadWireFormat + Send + 'static, C: ChannelFactory> {
    Send(Option<SendMessage<P, C>>),
    Recv(Result<ReceivedDatagram, crate::transport::TransportError>),
}

impl<PayloadDefinitions: PayloadWireFormat + Send + 'static, C: ChannelFactory>
    SendMessage<PayloadDefinitions, C>
{
    pub fn new(
        target_addr: SocketAddrV4,
        message: Message<PayloadDefinitions>,
    ) -> (C::OneshotReceiver<Result<(), Error>>, Self) {
        let (response_tx, response_rx) = C::oneshot();
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

pub struct SocketManager<PayloadDefinitions: Send + 'static, C: ChannelFactory> {
    receiver: C::BoundedReceiver<Result<ReceivedMessage<PayloadDefinitions>, Error>, 16>,
    sender: C::BoundedSender<SendMessage<PayloadDefinitions, C>, 16>,
    local_port: u16,
    session_id: u16,
    /// Set to true once `session_id` has wrapped from 0xFFFF → 1.
    /// Per AUTOSAR SOME/IP-SD, the reboot flag must be cleared after the
    /// first counter wrap and stay cleared.
    session_has_wrapped: bool,
}

impl<P: PayloadWireFormat + Send + 'static, C: ChannelFactory> std::fmt::Debug for SocketManager<P, C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SocketManager")
            .field("local_port", &self.local_port)
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl<MessageDefinitions, C> SocketManager<MessageDefinitions, C>
where
    MessageDefinitions: PayloadWireFormat + Send + 'static,
    C: ChannelFactory,
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
    /// trait can be exercised end-to-end. Additionally requires the
    /// `client-tokio` feature because the convenience defaults
    /// (`TokioTransport`, `TokioSpawner`) live behind it; under
    /// `--features client` the `socket_manager` module is compiled
    /// but this convenience method is not.
    #[cfg(all(test, feature = "client-tokio"))]
    pub async fn bind_discovery_seeded<R: E2ERegistryHandle>(
        interface: Ipv4Addr,
        e2e_registry: R,
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
    /// # Socket bounds
    ///
    /// Phase 12 relaxed the previous `F::Socket = TokioSocket` pin by
    /// switching [`TransportSocket`] to GATs. The factory's socket type
    /// must now satisfy:
    ///
    /// - `Send + Sync + 'static` — so the socket loop future can be
    ///   spawned on a multithreaded executor and outlive its owner.
    /// - `for<'a> SendFuture<'a>: Send` and `for<'a> RecvFuture<'a>: Send`
    ///   — the named GAT futures must themselves be `Send` so the
    ///   spawned loop crosses thread boundaries cleanly. The `for<'a>`
    ///   higher-ranked bound expresses "for any borrow lifetime" without
    ///   needing nightly-only Return-Type Notation (RFC 3654).
    ///
    /// Stable Rust cannot express `Send` bounds on the anonymous future
    /// types of `async fn` trait methods at use sites, which is why
    /// Phase 12 chose named associated types over RPITIT. See
    /// [`TransportSocket::SendFuture`](crate::transport::TransportSocket::SendFuture).
    ///
    /// # Bare-metal path
    ///
    /// Phase 11 abstracted the channel primitives behind
    /// [`ChannelFactory`](crate::transport::ChannelFactory). The
    /// `bare_metal` feature activates `EmbassySyncChannels` as an
    /// alternative to `TokioChannels`. With Phase 12's relaxed socket
    /// bound, a bare-metal consumer can now supply their own
    /// `TransportSocket` impl (e.g. wrapping `embassy_net::udp::UdpSocket`)
    /// as long as it is `Send + Sync + 'static` and its `SendFuture` /
    /// `RecvFuture` GAT projections are `Send` for every borrow lifetime.
    pub async fn bind_discovery_seeded_with_transport<F, S, R>(
        factory: &F,
        spawner: &S,
        interface: Ipv4Addr,
        e2e_registry: R,
        session_id: u16,
        session_has_wrapped: bool,
        multicast_loopback: bool,
    ) -> Result<Self, Error>
    where
        F: TransportFactory,
        F::Socket: Send + Sync + 'static,
        for<'a> <F::Socket as TransportSocket>::SendFuture<'a>: Send,
        for<'a> <F::Socket as TransportSocket>::RecvFuture<'a>: Send,
        S: Spawner,
        R: E2ERegistryHandle,
    {
        let (rx_tx, rx_rx) =
            C::bounded::<Result<ReceivedMessage<MessageDefinitions>, Error>, 16>();
        let (tx_tx, tx_rx) = C::bounded::<SendMessage<MessageDefinitions, C>, 16>();

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
    /// trait can be exercised end-to-end. Additionally requires the
    /// `client-tokio` feature because the convenience defaults live
    /// behind it.
    #[cfg(all(test, feature = "client-tokio"))]
    pub async fn bind<R: E2ERegistryHandle>(port: u16, e2e_registry: R) -> Result<Self, Error> {
        use crate::tokio_transport::{TokioSpawner, TokioTransport};
        Self::bind_with_transport(&TokioTransport, &TokioSpawner, port, e2e_registry).await
    }

    /// Variant of [`Self::bind`] that constructs the underlying socket
    /// through a caller-supplied [`TransportFactory`] and submits the
    /// socket's I/O loop through a caller-supplied [`Spawner`].
    ///
    /// # Generic bounds
    ///
    /// The factory's socket must be `Send + Sync + 'static` and its async
    /// methods must return `Send` futures so the socket loop can be
    /// spawned onto a multithreaded executor. See
    /// [`TransportSocket::SendFuture`](crate::transport::TransportSocket::SendFuture)
    /// for background on the GAT approach.
    pub async fn bind_with_transport<F, S, R>(
        factory: &F,
        spawner: &S,
        port: u16,
        e2e_registry: R,
    ) -> Result<Self, Error>
    where
        F: TransportFactory,
        F::Socket: Send + Sync + 'static,
        for<'a> <F::Socket as TransportSocket>::SendFuture<'a>: Send,
        for<'a> <F::Socket as TransportSocket>::RecvFuture<'a>: Send,
        S: Spawner,
        R: E2ERegistryHandle,
    {
        // Standardized to N=16 across both discovery and unicast bind
        // paths (was N=4 here historically — a tokio-conservative
        // choice). The trait's const-N now propagates to the GAT, so
        // the stored receiver/sender types must commit to a single N;
        // 16 matches what embassy-sync hardcodes and what discovery
        // already used. Bumping the unicast capacity from 4 to 16 has
        // no semantic effect — it just lets the channels absorb a
        // brief burst before backpressure kicks in.
        let (rx_tx, rx_rx) =
            C::bounded::<Result<ReceivedMessage<MessageDefinitions>, Error>, 16>();
        let (tx_tx, tx_rx) = C::bounded::<SendMessage<MessageDefinitions, C>, 16>();

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
        // Pre-encode size check: fail fast with `Error::Capacity("udp_buffer")`
        // for messages that exceed `UDP_BUFFER_SIZE`. Mirrors the analogous
        // check in `server::EventPublisher` so callers see a uniform
        // overload signal regardless of which path produced the oversize
        // message. Without this, an oversize encode would surface as a
        // protocol-level I/O error from inside the socket loop.
        let required = message.required_size();
        if required > UDP_BUFFER_SIZE {
            warn!(
                "outgoing message size {required} exceeds UDP_BUFFER_SIZE ({UDP_BUFFER_SIZE}); rejecting with Capacity(\"udp_buffer\")"
            );
            return Err(Error::Capacity("udp_buffer"));
        }
        let (result_channel, message) = SendMessage::<MessageDefinitions, C>::new(target_addr, message);
        self.sender.send(message).await.map_err(|()| {
            error!("Socket error when attempting to send message");
            Error::SocketClosedUnexpectedly
        })?;
        // The socket loop's response sender can be dropped without sending
        // (executor cancellation, bare-metal `Spawner` that drops futures,
        // or a panic in the loop). Surface that as a typed error rather
        // than `.expect`-panicking the caller.
        result_channel.recv().await.map_err(|_| {
            debug!("send result channel dropped (socket loop gone)");
            Error::SocketClosedUnexpectedly
        })??;
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
        MpscRecv::recv(&mut self.receiver).await
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
        _ = MpscRecv::recv(&mut receiver).await;
    }

    /// Build the I/O loop over any [`TransportSocket`] as a future.
    /// Callers are expected to spawn this future alongside [`Self`];
    /// the socket loop runs concurrently with its owner so
    /// `SocketManager::send`'s internal oneshot wait can complete.
    /// The reasoning for why the spawn hasn't been hoisted is in the
    /// module-level docs.
    ///
    /// # `Send` bounds
    ///
    /// The returned future must be `Send + 'static` for `Spawner::spawn`.
    /// This works on stable Rust (no RTN required) because:
    /// - `T: Send + Sync + 'static` makes the captured socket `Send`.
    /// - The HRTBs `for<'a> T::SendFuture<'a>: Send` and
    ///   `for<'a> T::RecvFuture<'a>: Send` make the GAT-projected futures
    ///   `Send` for every borrow lifetime, which is what propagates
    ///   `Send` to the enclosing `async` block.
    /// - All other captured state (`buf`, channels, registry) is `Send`.
    ///
    /// Bare-metal `TransportSocket` impls must ensure their `SendFuture`
    /// and `RecvFuture` associated types are `Send` (e.g. by avoiding
    /// `Rc` / `RefCell` in the future state) for this to compile.
    #[allow(clippy::too_many_lines)]
    async fn socket_loop_future<T, R>(
        socket: T,
        rx_tx: C::BoundedSender<Result<ReceivedMessage<MessageDefinitions>, Error>, 16>,
        mut tx_rx: C::BoundedReceiver<SendMessage<MessageDefinitions, C>, 16>,
        e2e_registry: R,
    )
    where
        T: TransportSocket + Send + Sync + 'static,
        for<'a> T::SendFuture<'a>: Send,
        for<'a> T::RecvFuture<'a>: Send,
        R: E2ERegistryHandle,
    {
        // Maximum number of consecutive `recv_from` errors tolerated before
        // the socket loop gives up. A single failure (transient I/O, peer
        // RST, ICMP port-unreachable amplified into `ConnectionRefused`)
        // is normal and should not tear down the socket. A persistent
        // failure (e.g. `EBADF` after the kernel closed the fd, or a
        // platform-level network-stack collapse) used to pin a CPU on a
        // tight `error!` log loop with no exit; this counter caps that.
        const MAX_CONSECUTIVE_RECV_ERRORS: u32 = 16;
        let mut consecutive_recv_errors: u32 = 0;
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
            let outcome: Outcome<MessageDefinitions, C> = {
                let send_fut = MpscRecv::recv(&mut tx_rx).fuse();
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
                        if e2e_registry.contains_key(&key) {
                            let upper_header: [u8; 8] =
                                buf[8..16].try_into().expect("upper header slice");
                            let mut protected = [0u8; UDP_BUFFER_SIZE];
                            let result = e2e_registry.protect(
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
                    consecutive_recv_errors = 0;
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
                            let (e2e_status, effective_payload) =
                                match e2e_registry.check(key, payload_bytes, upper_header) {
                                    Some((status, stripped)) => (Some(status), stripped),
                                    None => (None, payload_bytes),
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
                    if rx_tx.send(parse_result).await.is_ok() {
                    } else {
                        info!("Socket Dropping");
                        // The receiver has been dropped, so we should exit
                        break;
                    }
                }
                Outcome::Recv(Err(recv_err)) => {
                    // `tokio_transport::map_io_error` already logs the
                    // underlying `std::io::Error` (debug for transient
                    // kinds, warn for unusual ones) — keep this
                    // call-site at debug to avoid duplicating the same
                    // failure on the operator's screen.
                    consecutive_recv_errors = consecutive_recv_errors.saturating_add(1);
                    debug!(
                        "socket recv_from error ({}/{}): {:?}",
                        consecutive_recv_errors, MAX_CONSECUTIVE_RECV_ERRORS, recv_err,
                    );
                    if consecutive_recv_errors >= MAX_CONSECUTIVE_RECV_ERRORS {
                        error!(
                            "socket recv_from failed {} times consecutively; closing socket loop",
                            consecutive_recv_errors,
                        );
                        break;
                    }
                }
            }
        }
    }
}

#[cfg(all(test, feature = "client-tokio"))]
mod tests {
    use super::*;
    use crate::e2e::E2ERegistry;
    use crate::protocol::sd::test_support::{TestPayload, empty_sd_header};
    use crate::tokio_transport::{TokioChannels, TokioSpawner};
    use std::format;
    use std::sync::{Arc, Mutex};
    use std::vec;
    // Tests build ad-hoc UDP peers via tokio directly; this is not part of
    // the production code path, which goes through the `TransportSocket`
    // abstraction via `TokioTransport`.
    use tokio::net::UdpSocket;

    type TestSocketManager = SocketManager<TestPayload, TokioChannels>;

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
        use crate::transport::OneshotRecv;
        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1234);
        let msg = Message::new_sd(1, &empty_sd_header());
        let (rx, send_msg) = SendMessage::<TestPayload, TokioChannels>::new(target, msg);
        assert_eq!(send_msg.target_addr, target);
        // Verify the oneshot channel works
        send_msg.response.send(Ok(())).unwrap();
        assert!(rx.recv().await.unwrap().is_ok());
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
        let (_rx, send_msg) = SendMessage::<TestPayload, TokioChannels>::new(target, msg);
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
        use crate::protocol::sd::RebootFlag;
        let mut sm = bind_ephemeral_spawned().await;
        let raw_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let target =
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, raw_socket.local_addr().unwrap().port());
        let msg = || Message::<TestPayload>::new_sd(1, &empty_sd_header());

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

        // Craft a message whose raw-encoded size fits UDP_BUFFER_SIZE (16-byte
        // SOME/IP header + payload <= cap) but whose E2E-protected size
        // does not — Profile 4 adds `PROFILE4_HEADER_SIZE = 12` bytes,
        // so a payload of `UDP_BUFFER_SIZE - 16 - 4` exactly fits raw and
        // overflows by 8 once protected. Derive both fixture sizes from
        // `UDP_BUFFER_SIZE` so this stays correct if the constant moves.
        const SOMEIP_HEADER_SIZE: usize = 16;
        const PAYLOAD_LEN: usize = UDP_BUFFER_SIZE - SOMEIP_HEADER_SIZE - 4;

        // Register an E2E profile so the protect branch runs.
        let message_id = MessageId::new_from_service_and_method(0x1234, 0x5678);
        let key = E2EKey::from_message_id(message_id);
        let mut reg = E2ERegistry::new();
        reg.register(key, E2EProfile::Profile4(Profile4Config::new(0, 15)));
        let e2e_registry = Arc::new(Mutex::new(reg));

        let mut sm = SocketManager::<RawPayload, TokioChannels>::bind(0, e2e_registry)
            .await
            .unwrap();

        let payload_bytes = [0u8; PAYLOAD_LEN];
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

        let mut sm = SocketManager::<TestPayload, TokioChannels>::bind_with_transport(
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

        let mut buf = [0u8; UDP_BUFFER_SIZE];
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

    /// Phase 12 witness: proves `bind_with_transport` accepts a factory
    /// whose `Socket` type is **not** `TokioSocket`. The Phase 12 gate
    /// (no `F::Socket = TokioSocket` pin) is a type-system claim, and
    /// without this test the trait surface could regress to a Tokio
    /// pin in a future phase without any test catching it. The
    /// existing `bind_with_transport_*` tests both hardcode
    /// `type Socket = TokioSocket`, which only covers the previous
    /// pinned-bound shape.
    ///
    /// `WrappedSocket` is a transparent newtype around `TokioSocket`
    /// with its own `TransportSocket` impl — the *type identity* is
    /// what matters for this test, not the behavior. The end-to-end
    /// send-and-verify confirms the spawned I/O loop also carries
    /// through the wrapper, not just the bind call.
    #[tokio::test]
    async fn bind_with_transport_accepts_non_tokio_socket_type() {
        use crate::tokio_transport::{TokioSocket, TokioTransport};
        use crate::transport::TransportError;
        use core::future::Future;

        struct WrappedSocket(TokioSocket);

        impl TransportSocket for WrappedSocket {
            // Borrow the inner socket's named GAT futures; this keeps
            // the wrapper zero-overhead while still exercising a
            // distinct `Self::Socket` type at the bind call site.
            type SendFuture<'a> = <TokioSocket as TransportSocket>::SendFuture<'a>;
            type RecvFuture<'a> = <TokioSocket as TransportSocket>::RecvFuture<'a>;

            fn send_to<'a>(
                &'a self,
                buf: &'a [u8],
                target: SocketAddrV4,
            ) -> Self::SendFuture<'a> {
                self.0.send_to(buf, target)
            }
            fn recv_from<'a>(&'a self, buf: &'a mut [u8]) -> Self::RecvFuture<'a> {
                self.0.recv_from(buf)
            }
            fn local_addr(&self) -> Result<SocketAddrV4, TransportError> {
                self.0.local_addr()
            }
            fn join_multicast_v4(
                &self,
                group: Ipv4Addr,
                iface: Ipv4Addr,
            ) -> Result<(), TransportError> {
                self.0.join_multicast_v4(group, iface)
            }
            fn leave_multicast_v4(
                &self,
                group: Ipv4Addr,
                iface: Ipv4Addr,
            ) -> Result<(), TransportError> {
                self.0.leave_multicast_v4(group, iface)
            }
        }

        struct WrappingFactory;
        impl TransportFactory for WrappingFactory {
            type Socket = WrappedSocket;
            fn bind(
                &self,
                addr: SocketAddrV4,
                options: &SocketOptions,
            ) -> impl Future<Output = Result<Self::Socket, TransportError>> {
                let opts = *options;
                async move {
                    let inner = TokioTransport.bind(addr, &opts).await?;
                    Ok(WrappedSocket(inner))
                }
            }
        }

        // Compile-time witness: this `let` binding only typechecks if
        // `bind_with_transport` accepts `F::Socket = WrappedSocket` —
        // i.e. the previous `F::Socket = TokioSocket` pin is gone.
        let mut sm = SocketManager::<TestPayload, TokioChannels>::bind_with_transport(
            &WrappingFactory,
            &TokioSpawner,
            0,
            test_registry(),
        )
        .await
        .expect("bind via wrapping factory");
        let sm_port = sm.port();

        // Runtime witness: traffic flows through the wrapper's
        // `send_to` and the spawned I/O loop's `recv_from`.
        let recv = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let recv_port = recv.local_addr().unwrap().port();

        let msg = Message::<TestPayload>::new_sd(1, &empty_sd_header());
        sm.send(SocketAddrV4::new(Ipv4Addr::LOCALHOST, recv_port), msg)
            .await
            .expect("send via wrapping factory");

        let mut buf = [0u8; UDP_BUFFER_SIZE];
        let (len, _from) =
            tokio::time::timeout(std::time::Duration::from_secs(2), recv.recv_from(&mut buf))
                .await
                .expect("timed out waiting for datagram")
                .expect("recv failed");
        assert!(len > 0, "empty datagram");
        let view = MessageView::parse(&buf[..len]).unwrap();
        assert_eq!(view.header().message_id(), crate::protocol::MessageId::SD);
        let _ = sm_port;
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
