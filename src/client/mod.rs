//! SOME/IP client.
//!
//! # Memory footprint
//!
//! The client's internal `Inner` state is allocated inline rather than on
//! the heap. With the default capacity constants declared in `inner.rs` —
//! `REQUEST_QUEUE_CAP=32`, `PENDING_RESPONSES_CAP=64`, `UNICAST_SOCKETS_CAP=8`,
//! and `SESSION_CAP=64` — `Inner<P>` occupies on the order of **8–12 KiB**,
//! depending on `sizeof::<P>()` and `sizeof::<SocketManager<P>>()`.
//!
//! In addition, each `SocketManager`'s spawn loop holds a persistent
//! `[u8; UDP_BUFFER_SIZE]` receive/send buffer. When the send path needs
//! E2E protection (i.e. the destination key is registered in the
//! `E2ERegistry`), it transiently allocates a second
//! `[u8; UDP_BUFFER_SIZE]` on the stack for the protected output; sends
//! without E2E protection do not pay this cost. So an active
//! socket-loop future carries one always-live `UDP_BUFFER_SIZE` buffer
//! plus up to one additional `UDP_BUFFER_SIZE` buffer during E2E sends.
//! With `UNICAST_SOCKETS_CAP=8` sockets bound, the total per-client
//! buffer budget scales as `UNICAST_SOCKETS_CAP * UDP_BUFFER_SIZE`
//! always-live, up to `2 * UNICAST_SOCKETS_CAP * UDP_BUFFER_SIZE` at
//! peak during concurrent E2E-protected sends on every socket. At the
//! current default of `UDP_BUFFER_SIZE = 1500`, that is ~12 KiB
//! always-live / ~24 KiB peak per client.
//!
//! On `std + tokio`, all of this is allocated on the heap when each future
//! is spawned, so the overhead is invisible to callers. On the bare-metal
//! port (future), whoever drives the futures must arrange storage for them
//! (either a `static` or a heap allocator); the capacity constants plus
//! [`crate::UDP_BUFFER_SIZE`] are the knobs for trimming this footprint.
mod error;
mod inner;
mod service_registry;
mod session;
mod socket_manager;

pub use error::Error;

use crate::e2e::{E2ECheckStatus, E2EKey, E2EProfile, E2ERegistry};
use crate::{protocol, protocol::Message, traits::PayloadWireFormat};
use inner::{ControlMessage, Inner};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, Mutex, RwLock};
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
    /// Returns the same errors as the request itself (e.g. deserialization
    /// failure). Returns [`Error::Shutdown`] if the client's run-loop
    /// future exits before the response is delivered — the caller's
    /// `PendingResponse` handle outlived its driver.
    pub async fn response(self) -> Result<P, Error> {
        self.receiver.await.map_err(|_| Error::Shutdown)?
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
    ///
    /// When E2E is configured for this message's key, `e2e_status` contains
    /// the check result and the payload has its E2E header stripped.
    /// When no E2E is configured, `e2e_status` is `None`.
    Unicast {
        /// The received SOME/IP message.
        message: Message<P>,
        /// E2E check status, if E2E was configured for this message.
        e2e_status: Option<E2ECheckStatus>,
    },
    /// The client encountered an error.
    Error(Error),
}

impl<P: PayloadWireFormat> std::fmt::Debug for ClientUpdate<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DiscoveryUpdated(msg) => f.debug_tuple("DiscoveryUpdated").field(msg).finish(),
            Self::SenderRebooted(addr) => f.debug_tuple("SenderRebooted").field(addr).finish(),
            Self::Unicast {
                message,
                e2e_status,
            } => f
                .debug_struct("Unicast")
                .field("message", message)
                .field("e2e_status", e2e_status)
                .finish(),
            Self::Error(err) => f.debug_tuple("Error").field(err).finish(),
        }
    }
}

/// Stream of updates from the SOME/IP client event loop.
///
/// Returned by [`Client::new`]. Call [`recv`](Self::recv) to receive
/// discovery, unicast, and error updates.
pub struct ClientUpdates<MessageDefinitions: PayloadWireFormat> {
    update_receiver: mpsc::UnboundedReceiver<ClientUpdate<MessageDefinitions>>,
}

impl<MessageDefinitions: PayloadWireFormat> std::fmt::Debug for ClientUpdates<MessageDefinitions> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientUpdates").finish_non_exhaustive()
    }
}

impl<MessageDefinitions: PayloadWireFormat> ClientUpdates<MessageDefinitions> {
    /// Waits for the next update from the client event loop.
    ///
    /// Returns `None` when the inner loop has exited (all `Client` handles
    /// dropped and the event loop finished draining).
    pub async fn recv(&mut self) -> Option<ClientUpdate<MessageDefinitions>> {
        self.update_receiver.recv().await
    }
}

/// A SOME/IP client that handles service discovery and message exchange.
///
/// `Client` is cheaply [`Clone`]-able. All clones share the same underlying
/// event loop and can be used concurrently from different tasks.
#[derive(Clone)]
pub struct Client<MessageDefinitions: PayloadWireFormat> {
    interface: Arc<RwLock<Ipv4Addr>>,
    control_sender: mpsc::Sender<inner::ControlMessage<MessageDefinitions>>,
    e2e_registry: Arc<Mutex<E2ERegistry>>,
}

impl<MessageDefinitions: PayloadWireFormat> std::fmt::Debug for Client<MessageDefinitions> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field(
                "interface",
                &*self.interface.read().expect("interface lock poisoned"),
            )
            .finish_non_exhaustive()
    }
}

impl<MessageDefinitions> Client<MessageDefinitions>
where
    MessageDefinitions: PayloadWireFormat + Clone + std::fmt::Debug + 'static,
{
    /// Creates a new client bound to the given network interface and returns its run-loop future to be driven by the caller.
    ///
    /// Returns a `(Client, ClientUpdates, run_future)` triple. The `Client`
    /// handle is [`Clone`]-able and can be shared across tasks.
    /// `ClientUpdates` receives discovery, unicast, and error updates from
    /// the event loop. `run_future` is the event loop itself — the caller
    /// must drive it to completion (typically via `tokio::spawn`) for the
    /// client to process any messages.
    ///
    /// The future is bounded `Send + 'static` because every in-repo
    /// consumer spawns it on a multithreaded executor. Bare-metal
    /// consumers whose transport produces `!Send` state will get a
    /// cfg-gated alternative constructor alongside the bare-metal port.
    ///
    /// ```no_run
    /// # use simple_someip::{Client, RawPayload};
    /// # use std::net::Ipv4Addr;
    /// # async fn demo() {
    /// let (client, mut updates, run) = Client::<RawPayload>::new(Ipv4Addr::LOCALHOST);
    /// tokio::spawn(run);
    /// // ...interact with `client` and `updates`...
    /// # let _ = (client, updates);
    /// # }
    /// ```
    #[must_use = "the returned run-loop future must be spawned (e.g. tokio::spawn) for the client to make progress"]
    pub fn new(
        interface: Ipv4Addr,
    ) -> (
        Self,
        ClientUpdates<MessageDefinitions>,
        impl core::future::Future<Output = ()> + Send + 'static,
    ) {
        Self::new_with_loopback(interface, false)
    }

    /// Like [`Self::new`], but with explicit control over multicast loopback.
    ///
    /// When `multicast_loopback` is `true`, SD messages sent by this client
    /// are looped back to other sockets on the same host. This is required
    /// when running both a client and a server/simulator on the same machine
    /// for testing. Defaults to `false` in [`Self::new`].
    ///
    /// # Loopback caveat
    ///
    /// With loopback enabled, the client's own discovery socket also receives
    /// the multicast SD traffic this client sends (e.g. `FindService` probes
    /// and periodic `OfferService` announcements driven by
    /// [`Self::start_sd_announcements`]). Those self-sent messages are parsed
    /// the same as any other inbound SD traffic, so callers may observe:
    ///
    /// - [`ClientUpdate::DiscoveryUpdated`] events originating from this
    ///   client's own IP/port, and
    /// - self-advertised services appearing in the internal discovery
    ///   registry.
    ///
    /// Consumers of [`ClientUpdates`] that need to ignore self-sent SD should
    /// filter on source address (the sender's IP/port is included on the
    /// update).
    #[must_use = "the returned run-loop future must be spawned (e.g. tokio::spawn) for the client to make progress"]
    pub fn new_with_loopback(
        interface: Ipv4Addr,
        multicast_loopback: bool,
    ) -> (
        Self,
        ClientUpdates<MessageDefinitions>,
        impl core::future::Future<Output = ()> + Send + 'static,
    ) {
        let e2e_registry = Arc::new(Mutex::new(E2ERegistry::new()));
        let (control_sender, update_receiver, run_future) =
            Inner::build(interface, Arc::clone(&e2e_registry), multicast_loopback);

        let client = Self {
            interface: Arc::new(RwLock::new(interface)),
            control_sender,
            e2e_registry,
        };
        let updates = ClientUpdates { update_receiver };
        (client, updates, run_future)
    }

    /// Returns the current network interface address.
    ///
    /// # Panics
    ///
    /// Panics if the interface lock is poisoned.
    #[must_use]
    pub fn interface(&self) -> Ipv4Addr {
        *self.interface.read().expect("interface lock poisoned")
    }

    /// Changes the network interface and rebinds sockets.
    ///
    /// # Errors
    ///
    /// Returns an error if rebinding sockets on the new interface fails.
    ///
    /// Returns [`Error::Shutdown`] if the client's run-loop future has
    /// exited before this call — the control-channel send cannot
    /// complete without its receiver.
    ///
    /// # Panics
    ///
    /// Panics if the interface lock is poisoned (indicates prior panic
    /// while the lock was held).
    pub async fn set_interface(&self, interface: Ipv4Addr) -> Result<(), Error> {
        let (response, message) = ControlMessage::set_interface(interface);
        self.control_sender
            .send(message)
            .await
            .map_err(|_| Error::Shutdown)?;
        response.await.map_err(|_| Error::Shutdown)??;
        *self.interface.write().expect("interface lock poisoned") = interface;
        Ok(())
    }

    /// Binds the SD multicast discovery socket.
    ///
    /// # Errors
    ///
    /// Returns an error if binding the multicast socket fails.
    /// Returns [`Error::Shutdown`] if the client's run-loop future has
    /// exited before this call (dropped, cancelled, or otherwise gone)
    /// — the `Client` handle has outlived its driver and further
    /// control-channel sends cannot make progress.
    pub async fn bind_discovery(&self) -> Result<(), Error> {
        let (response, message) = ControlMessage::bind_discovery();
        self.control_sender
            .send(message)
            .await
            .map_err(|_| Error::Shutdown)?;
        response.await.map_err(|_| Error::Shutdown)?
    }

    /// Unbinds the SD multicast discovery socket.
    ///
    /// # Errors
    ///
    /// Returns an error if unbinding the multicast socket fails.
    /// Returns [`Error::Shutdown`] if the client's run-loop future has
    /// exited before this call (dropped, cancelled, or otherwise gone)
    /// — the `Client` handle has outlived its driver and further
    /// control-channel sends cannot make progress.
    pub async fn unbind_discovery(&self) -> Result<(), Error> {
        let (response, message) = ControlMessage::unbind_discovery();
        self.control_sender
            .send(message)
            .await
            .map_err(|_| Error::Shutdown)?;
        response.await.map_err(|_| Error::Shutdown)?
    }

    /// Subscribes to an event group on a known service.
    ///
    /// # Errors
    ///
    /// Returns an error if the service is not found or subscription fails.
    /// Returns [`Error::Shutdown`] if the client's run-loop future has
    /// exited before this call (dropped, cancelled, or otherwise gone)
    /// — the `Client` handle has outlived its driver and further
    /// control-channel sends cannot make progress.
    pub async fn subscribe(
        &self,
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
        self.control_sender
            .send(message)
            .await
            .map_err(|_| Error::Shutdown)?;
        response.await.map_err(|_| Error::Shutdown)?
    }

    /// Like [`subscribe`](Self::subscribe) but does not wait for the
    /// subscription result.
    ///
    /// This still awaits enqueueing the control message on the internal
    /// channel, so it may block if that bounded channel is full. Useful for
    /// periodic renewals where waiting for subscription processing is
    /// unnecessary.
    pub async fn subscribe_no_wait(
        &self,
        service_id: u16,
        instance_id: u16,
        major_version: u8,
        ttl: u32,
        event_group_id: u16,
        client_port: u16,
    ) {
        let (response, message) = ControlMessage::subscribe(
            service_id,
            instance_id,
            major_version,
            ttl,
            event_group_id,
            client_port,
        );
        let _ = self.control_sender.send(message).await;
        // Consume the response in the background so the inner loop doesn't
        // warn about a dropped receiver.
        tokio::spawn(async move {
            let _ = response.await;
        });
    }

    /// Returns the current SD reboot flag tracked by the client.
    ///
    /// Per AUTOSAR SOME/IP-SD, the reboot flag is
    /// [`RebootFlag::RecentlyRebooted`](protocol::sd::RebootFlag::RecentlyRebooted)
    /// from startup until the session counter wraps from `0xFFFF` to `1`, then
    /// [`RebootFlag::Continuous`](protocol::sd::RebootFlag::Continuous) permanently.
    ///
    /// While discovery is bound, the returned value is the discovery socket's
    /// live reboot flag. While discovery is **unbound**, the inner loop's
    /// persisted wrap state is used instead — so this method correctly returns
    /// [`RebootFlag::Continuous`](protocol::sd::RebootFlag::Continuous) even
    /// between `unbind_discovery` and a subsequent `bind_discovery`, provided
    /// the session counter had already wrapped at least once. On a fresh
    /// client that has never bound discovery (or that unbound before any
    /// wrap),
    /// [`RebootFlag::RecentlyRebooted`](protocol::sd::RebootFlag::RecentlyRebooted)
    /// is returned.
    ///
    /// Call this before manually building an SD header (e.g. one passed to
    /// [`send_sd_message`](Self::send_sd_message)) so the reboot flag reflects
    /// the current tracked state instead of a stale value baked at call time.
    /// Headers passed to [`start_sd_announcements`](Self::start_sd_announcements)
    /// are refreshed automatically per-tick and do not need this call.
    ///
    /// # Panics
    ///
    /// Panics if the internal control channel is closed.
    pub async fn reboot_flag(&self) -> protocol::sd::RebootFlag {
        let (response, message) = ControlMessage::query_reboot_flag();
        self.control_sender.send(message).await.unwrap();
        response.await.unwrap()
    }

    /// Test-only: force the inner loop's `sd_session_has_wrapped` so tests
    /// can observe post-wrap behavior without sending 65k SD messages.
    #[cfg(test)]
    pub(crate) async fn force_sd_session_wrapped_for_test(&self, wrapped: bool) {
        let (response, message) = ControlMessage::force_sd_session_wrapped_for_test(wrapped);
        self.control_sender.send(message).await.unwrap();
        response.await.unwrap();
    }

    /// Sends an SD message to a specific target address.
    ///
    /// # Errors
    ///
    /// Returns an error if sending the SD message fails.
    /// Returns [`Error::Shutdown`] if the client's run-loop future has
    /// exited before this call (dropped, cancelled, or otherwise gone)
    /// — the `Client` handle has outlived its driver and further
    /// control-channel sends cannot make progress.
    pub async fn send_sd_message(
        &self,
        target: SocketAddrV4,
        sd_header: <MessageDefinitions as PayloadWireFormat>::SdHeader,
    ) -> Result<(), Error> {
        let (response, message) = ControlMessage::send_sd(target, sd_header);
        self.control_sender
            .send(message)
            .await
            .map_err(|_| Error::Shutdown)?;
        response.await.map_err(|_| Error::Shutdown)?
    }

    /// Start periodic SD announcements on the client's discovery socket.
    ///
    /// Spawns a background task that sends the given SD header to the
    /// multicast group at a regular interval. Use this to bundle
    /// `FindService` + `OfferService` entries from a single SD identity
    /// when the application acts as both client and server.
    ///
    /// The announcements are sent via the client's SD socket, ensuring
    /// they share the same source address as the client's `Subscribe` and
    /// `FindService` messages.
    ///
    /// **Reboot flag auto-refresh:** the SD header's reboot bit is overridden
    /// at each tick with the client's currently tracked reboot flag (via
    /// [`PayloadWireFormat::set_reboot_flag`]). The reboot bit the caller
    /// supplies on `sd_header` is therefore ignored. This ensures the flag
    /// transitions from `RecentlyRebooted` to `Continuous` once the session
    /// counter wraps past `0xFFFF`, rather than staying stuck on whatever
    /// value was baked at call time.
    ///
    /// Returns a [`tokio::task::JoinHandle`] that can be used to abort the
    /// background task. The task uses a weak reference to the client's
    /// control channel, so it exits automatically when all `Client` handles
    /// are dropped (via `shut_down()` or going out of scope).
    ///
    /// # Arguments
    ///
    /// * `sd_header` — The SD header to send (entries + options).
    /// * `interval` — How often to send (e.g. every 1 second). Values below
    ///   100ms are clamped to 100ms to prevent tight loops.
    pub fn start_sd_announcements(
        &self,
        sd_header: <MessageDefinitions as PayloadWireFormat>::SdHeader,
        interval: std::time::Duration,
    ) -> tokio::task::JoinHandle<()>
    where
        <MessageDefinitions as PayloadWireFormat>::SdHeader: Send + 'static,
    {
        use crate::protocol::sd;

        // Use a WeakSender so this task does NOT keep the control channel
        // alive. When all strong Client handles are dropped (shut_down),
        // the weak sender will fail to upgrade and the task exits cleanly.
        let weak_sender = self.control_sender.downgrade();
        let target = SocketAddrV4::new(sd::MULTICAST_IP, sd::MULTICAST_PORT);
        let interval = interval.max(std::time::Duration::from_millis(100));

        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // Consume the immediate first tick so we don't send before
            // the caller has finished setting up (e.g. subscribing).
            tick.tick().await;
            let mut count = 0u64;
            loop {
                tick.tick().await;

                // Refresh the reboot flag from the client's tracked state
                // so long-running announcers transition from RecentlyRebooted
                // to Continuous once the session counter wraps. The weak
                // sender is upgraded, used to enqueue a single control
                // message, then dropped before we await — keeping the strong
                // sender alive across awaits would defeat the weak-sender
                // shutdown path.
                let (flag_rx, flag_msg) = ControlMessage::query_reboot_flag();
                let Some(sender) = weak_sender.upgrade() else {
                    tracing::info!("Client shut down, stopping SD announcements");
                    break;
                };
                let enqueue_ok = sender.send(flag_msg).await.is_ok();
                drop(sender);
                if !enqueue_ok {
                    tracing::warn!("SD announcement channel closed, stopping");
                    break;
                }
                let Ok(reboot) = flag_rx.await else {
                    tracing::warn!("SD announcement reboot-flag query dropped, stopping");
                    break;
                };
                let mut header = sd_header.clone();
                MessageDefinitions::set_reboot_flag(&mut header, reboot);

                let (response, message) = ControlMessage::send_sd(target, header);

                let Some(sender) = weak_sender.upgrade() else {
                    tracing::info!("Client shut down, stopping SD announcements");
                    break;
                };
                let send_ok = sender.send(message).await.is_ok();
                drop(sender);

                if !send_ok {
                    tracing::warn!("SD announcement channel closed, stopping");
                    break;
                }

                match response.await {
                    Ok(Ok(())) => {
                        count += 1;
                        if count == 1 {
                            tracing::info!("Sent first client SD announcement");
                        } else {
                            tracing::trace!("Sent {count} client SD announcements");
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::error!("Failed to send SD announcement: {e:?}");
                    }
                    Err(_) => {
                        tracing::warn!("SD announcement response dropped, stopping");
                        break;
                    }
                }
            }
        })
    }

    /// Registers a service endpoint in the client's endpoint registry.
    ///
    /// `local_port` controls which source port is used when sending to this
    /// endpoint via [`send_to_service`](Self::send_to_service). Pass `0` to
    /// use an ephemeral (OS-assigned) port.
    ///
    /// Service-discovery (SD) automatically populates endpoints with
    /// `local_port = 0`. If your configuration requires a specific source
    /// port, you must call `add_endpoint` explicitly — even if SD has already
    /// registered the service — so that the correct `local_port` is stored.
    ///
    /// # Errors
    ///
    /// Returns an error if registering the endpoint fails.
    /// Returns [`Error::Shutdown`] if the client's run-loop future has
    /// exited before this call (dropped, cancelled, or otherwise gone)
    /// — the `Client` handle has outlived its driver and further
    /// control-channel sends cannot make progress.
    pub async fn add_endpoint(
        &self,
        service_id: u16,
        instance_id: u16,
        addr: SocketAddrV4,
        local_port: u16,
    ) -> Result<(), Error> {
        let (response, message) =
            ControlMessage::add_endpoint(service_id, instance_id, addr, local_port);
        self.control_sender
            .send(message)
            .await
            .map_err(|_| Error::Shutdown)?;
        response.await.map_err(|_| Error::Shutdown)?
    }

    /// Removes a service endpoint from the client's endpoint registry.
    ///
    /// # Errors
    ///
    /// Returns an error if removing the endpoint fails.
    /// Returns [`Error::Shutdown`] if the client's run-loop future has
    /// exited before this call (dropped, cancelled, or otherwise gone)
    /// — the `Client` handle has outlived its driver and further
    /// control-channel sends cannot make progress.
    pub async fn remove_endpoint(&self, service_id: u16, instance_id: u16) -> Result<(), Error> {
        let (response, message) = ControlMessage::remove_endpoint(service_id, instance_id);
        self.control_sender
            .send(message)
            .await
            .map_err(|_| Error::Shutdown)?;
        response.await.map_err(|_| Error::Shutdown)?
    }

    /// Sends a message to a service and returns a handle to await the response.
    ///
    /// Call `.response()` on the returned handle to await the reply payload.
    ///
    /// # Saturation behavior
    ///
    /// Response tracking uses a fixed-capacity internal map. If it is
    /// saturated at the moment the reply-tracking slot would be installed,
    /// this method still returns `Ok(PendingResponse)` — the UDP send has
    /// already happened — but the returned `PendingResponse` will resolve to
    /// `Err(Error::Capacity("pending_responses"))`. Any reply that later
    /// arrives for that `request_id` is delivered as
    /// [`ClientUpdate::Unicast`] on the update stream instead of through the
    /// `PendingResponse`. Treat this error as "reply lost to saturation",
    /// not "send failed". A `warn!`-level log accompanies the drop.
    ///
    /// # Errors
    ///
    /// Returns an error if the service is not found, unicast binding fails,
    /// or the UDP send fails.
    /// Returns [`Error::Shutdown`] if the client's run-loop future has
    /// exited before this call (dropped, cancelled, or otherwise gone)
    /// — the `Client` handle has outlived its driver and further
    /// control-channel sends cannot make progress.
    pub async fn send_to_service(
        &self,
        service_id: u16,
        instance_id: u16,
        message: crate::protocol::Message<MessageDefinitions>,
    ) -> Result<PendingResponse<MessageDefinitions>, Error> {
        let (send_rx, response_rx, ctrl_msg) =
            ControlMessage::send_to_service(service_id, instance_id, message);
        self.control_sender
            .send(ctrl_msg)
            .await
            .map_err(|_| Error::Shutdown)?;
        send_rx.await.map_err(|_| Error::Shutdown)??;
        Ok(PendingResponse {
            receiver: response_rx,
        })
    }

    /// Sends a request to a service and awaits the response in one call.
    ///
    /// Unlike [`send_to_service`](Self::send_to_service), this method does not
    /// require manually driving [`ClientUpdates::recv`] — the inner event loop
    /// resolves the response independently.
    ///
    /// # Errors
    ///
    /// Returns an error if the service is not found, unicast binding fails,
    /// the UDP send fails, or the response payload fails to deserialize.
    /// Returns [`Error::Shutdown`] if the client's run-loop future has
    /// exited before this call (dropped, cancelled, or otherwise gone)
    /// — the `Client` handle has outlived its driver and further
    /// control-channel sends cannot make progress.
    pub async fn request(
        &self,
        service_id: u16,
        instance_id: u16,
        message: crate::protocol::Message<MessageDefinitions>,
    ) -> Result<MessageDefinitions, Error> {
        let (send_rx, response_rx, ctrl_msg) =
            ControlMessage::send_to_service(service_id, instance_id, message);
        self.control_sender
            .send(ctrl_msg)
            .await
            .map_err(|_| Error::Shutdown)?;
        send_rx.await.map_err(|_| Error::Shutdown)??;
        response_rx.await.map_err(|_| Error::Shutdown)?
    }

    /// Register an E2E profile for the given key.
    ///
    /// Once registered, incoming messages matching `key` will have their E2E
    /// header checked and stripped, and outgoing messages will have E2E
    /// protection applied automatically.
    ///
    /// # Panics
    ///
    /// Panics if the E2E registry mutex is poisoned.
    pub fn register_e2e(&self, key: E2EKey, profile: E2EProfile) {
        self.e2e_registry
            .lock()
            .expect("e2e registry lock poisoned")
            .register(key, profile);
    }

    /// Remove E2E configuration for the given key.
    ///
    /// # Panics
    ///
    /// Panics if the E2E registry mutex is poisoned.
    pub fn unregister_e2e(&self, key: &E2EKey) {
        self.e2e_registry
            .lock()
            .expect("e2e registry lock poisoned")
            .unregister(key);
    }

    /// Shuts down the client by dropping the control channel.
    ///
    /// The inner event loop will exit once all `Client` clones are dropped.
    /// Remaining updates can be drained via [`ClientUpdates::recv`].
    pub fn shut_down(self) {
        drop(self.control_sender);
        info!("Shutting Down SOME/IP client");
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
        let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
        tokio::spawn(run_fut);
        assert_eq!(client.interface(), Ipv4Addr::LOCALHOST);
        client.shut_down();
    }

    #[tokio::test]
    async fn test_client_debug() {
        let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
        tokio::spawn(run_fut);
        let debug_str = format!("{client:?}");
        assert!(debug_str.contains("Client"));
        assert!(debug_str.contains("127.0.0.1"));
        client.shut_down();
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
        let update: ClientUpdate<TestPayload> = ClientUpdate::Unicast {
            message: msg,
            e2e_status: None,
        };
        let debug_str = format!("{update:?}");
        assert!(debug_str.contains("Unicast"));

        // Error
        let update: ClientUpdate<TestPayload> = ClientUpdate::Error(Error::ServiceNotFound);
        let debug_str = format!("{update:?}");
        assert!(debug_str.contains("Error"));
    }

    #[tokio::test]
    async fn test_subscribe_unknown_service_returns_error() {
        let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
        tokio::spawn(run_fut);
        let result = client.subscribe(0xFFFF, 0xFFFF, 1, 3, 0x01, 0).await;
        assert!(
            matches!(result, Err(Error::ServiceNotFound)),
            "expected ServiceNotFound, got {result:?}"
        );
        client.shut_down();
    }

    #[tokio::test]
    async fn test_subscribe_no_wait_unknown_service_does_not_panic() {
        let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
        tokio::spawn(run_fut);
        // subscribe_no_wait is fire-and-forget — it should not panic even
        // when the service is unknown (the inner loop sends ServiceNotFound
        // on the dropped response channel, which is harmless).
        client
            .subscribe_no_wait(0xFFFF, 0xFFFF, 1, 3, 0x01, 0)
            .await;
        client.shut_down();
    }

    #[tokio::test]
    async fn test_bind_discovery_and_unbind() {
        let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
        tokio::spawn(run_fut);
        client.bind_discovery().await.unwrap();
        client.unbind_discovery().await.unwrap();
        client.shut_down();
    }

    #[tokio::test]
    async fn test_set_interface() {
        let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
        tokio::spawn(run_fut);
        let new_addr = Ipv4Addr::LOCALHOST;
        client.set_interface(new_addr).await.unwrap();
        assert_eq!(client.interface(), new_addr);
        client.shut_down();
    }

    #[tokio::test]
    async fn test_add_endpoint_succeeds() {
        let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
        tokio::spawn(run_fut);
        let addr = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 1), 30000);
        client.add_endpoint(0x1234, 0x0001, addr, 0).await.unwrap();
        client.shut_down();
    }

    #[tokio::test]
    async fn test_send_to_service_unknown_returns_error() {
        let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
        tokio::spawn(run_fut);
        let msg = crate::protocol::Message::new_sd(1, &empty_sd_header());
        let result = client.send_to_service(0xFFFF, 0xFFFF, msg).await;
        assert!(
            matches!(result, Err(Error::ServiceNotFound)),
            "expected ServiceNotFound, got {result:?}"
        );
        client.shut_down();
    }

    #[tokio::test]
    async fn test_remove_endpoint_succeeds() {
        let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
        tokio::spawn(run_fut);
        let addr = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 1), 30000);
        client.add_endpoint(0x1234, 0x0001, addr, 0).await.unwrap();
        client.remove_endpoint(0x1234, 0x0001).await.unwrap();
        client.shut_down();
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
        let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
        tokio::spawn(run_fut);
        // Bind discovery first so the send path uses the existing socket
        client.bind_discovery().await.unwrap();
        let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 30490);
        let sd_header = empty_sd_header();
        client.send_sd_message(target, sd_header).await.unwrap();
        client.shut_down();
    }

    #[tokio::test]
    async fn test_send_to_service_success_returns_pending_response() {
        let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
        tokio::spawn(run_fut);
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 30000);
        client.add_endpoint(0x1234, 0x0001, addr, 0).await.unwrap();
        let msg = crate::protocol::Message::new_sd(1, &empty_sd_header());
        // send_to_service succeeds (send completes), returning a PendingResponse
        let pending = client.send_to_service(0x1234, 0x0001, msg).await;
        assert!(pending.is_ok());
        client.shut_down();
    }

    #[tokio::test]
    async fn test_recv_returns_none_after_shutdown() {
        let (client, mut updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
        tokio::spawn(run_fut);
        client.shut_down();
        // Now the inner loop should exit; recv() should return None
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), updates.recv()).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_register_and_unregister_e2e() {
        let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
        tokio::spawn(run_fut);
        let key = E2EKey {
            service_id: 0x1234,
            method_or_event_id: 0x0001,
        };
        let profile = E2EProfile::Profile4(crate::e2e::Profile4Config::new(42, 10));
        client.register_e2e(key, profile);
        client.unregister_e2e(&key);
        client.shut_down();
    }

    #[tokio::test]
    async fn test_client_is_clone() {
        let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
        tokio::spawn(run_fut);
        let client2 = client.clone();
        assert_eq!(client.interface(), client2.interface());
        client.shut_down();
    }

    #[tokio::test]
    async fn test_client_updates_debug() {
        let (_client, updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
        tokio::spawn(run_fut);
        let debug_str = format!("{updates:?}");
        assert!(debug_str.contains("ClientUpdates"));
    }

    #[tokio::test]
    async fn test_request_unknown_service_returns_error() {
        let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
        tokio::spawn(run_fut);
        let msg = crate::protocol::Message::new_sd(1, &empty_sd_header());
        let result = client.request(0xFFFF, 0xFFFF, msg).await;
        assert!(
            matches!(result, Err(Error::ServiceNotFound)),
            "expected ServiceNotFound, got {result:?}"
        );
        client.shut_down();
    }

    #[tokio::test]
    async fn test_start_sd_announcements_does_not_panic() {
        let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
        tokio::spawn(run_fut);
        client.bind_discovery().await.unwrap();

        let sd_header = empty_sd_header();
        let handle =
            client.start_sd_announcements(sd_header, std::time::Duration::from_millis(100));

        // Let the task fire at least once (may fail to send on loopback, that's OK).
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;

        handle.abort();
        let result = handle.await;
        let err = result.unwrap_err();
        assert!(
            err.is_cancelled(),
            "task should have been cancelled, not panicked"
        );

        client.shut_down();
    }

    #[tokio::test]
    async fn test_start_sd_announcements_without_discovery_bound() {
        let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
        tokio::spawn(run_fut);
        // Don't bind discovery — the task should handle the error gracefully.
        let sd_header = empty_sd_header();
        let handle =
            client.start_sd_announcements(sd_header, std::time::Duration::from_millis(100));

        tokio::time::sleep(std::time::Duration::from_millis(250)).await;

        handle.abort();
        let result = handle.await;
        let err = result.unwrap_err();
        assert!(
            err.is_cancelled(),
            "task should have been cancelled, not panicked"
        );

        client.shut_down();
    }

    #[tokio::test]
    async fn test_start_sd_announcements_abort_stops_task() {
        let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
        tokio::spawn(run_fut);
        client.bind_discovery().await.unwrap();

        let sd_header = empty_sd_header();
        let handle =
            client.start_sd_announcements(sd_header, std::time::Duration::from_millis(100));

        handle.abort();
        let result = handle.await;
        let err = result.unwrap_err();
        assert!(
            err.is_cancelled(),
            "task should have been cancelled, not panicked"
        );

        client.shut_down();
    }

    #[tokio::test]
    async fn test_start_sd_announcements_overrides_caller_reboot_flag() {
        // Regression test for the auto-refresh behavior: a caller who bakes
        // `Continuous` into `sd_header.flags` must still observe the client's
        // tracked flag on the wire (here, `RecentlyRebooted`, because the
        // session counter has not wrapped on a freshly-bound socket). This
        // verifies the announcer calls `set_reboot_flag` on each tick rather
        // than using the stale caller-supplied value.
        let (client, mut updates, run_fut) =
            TestClient::new_with_loopback(Ipv4Addr::LOCALHOST, true);
        tokio::spawn(run_fut);
        client.bind_discovery().await.unwrap();

        // Caller bakes in Continuous — the announcer must override this.
        let mut sd_header = empty_sd_header();
        sd_header.flags =
            crate::protocol::sd::Flags::new_sd(crate::protocol::sd::RebootFlag::Continuous);

        let handle =
            client.start_sd_announcements(sd_header, std::time::Duration::from_millis(100));

        // Loopback delivers our own SD announcements back as DiscoveryUpdated.
        // Drain updates until we see one (tokio::time::interval skips the
        // first immediate tick, so the first real send lands at ~100-200ms).
        let received = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match updates.recv().await {
                    Some(ClientUpdate::DiscoveryUpdated(msg)) => return Some(msg),
                    Some(_) => {}
                    None => return None,
                }
            }
        })
        .await
        .expect("timed out waiting for SD announcement")
        .expect("update stream closed");

        assert_eq!(
            received.sd_header.flags.reboot(),
            crate::protocol::sd::RebootFlag::RecentlyRebooted,
            "announcer should have overridden the caller-supplied Continuous \
             flag with the client's tracked RecentlyRebooted state"
        );

        handle.abort();
        let _ = handle.await;
        client.shut_down();
    }

    #[tokio::test]
    async fn test_reboot_flag_uses_persisted_wrap_state_when_unbound() {
        // Regression test for Copilot comment #5 on PR 73: when discovery
        // is not bound, `reboot_flag()` must consult the inner loop's
        // persisted `sd_session_has_wrapped` (set on every unbind from the
        // departing socket's reboot_flag) rather than blindly returning
        // `RecentlyRebooted`. Otherwise a long-running client that wrapped
        // past 0xFFFF would regress to `RecentlyRebooted` on the next
        // `reboot_flag()` call after unbind — falsely advertising a reboot
        // to peers on the next manually-built SD header.
        let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
        tokio::spawn(run_fut);

        // No discovery bound. Fallback should reflect persisted state.
        // Default (unwrapped) → RecentlyRebooted.
        assert_eq!(
            client.reboot_flag().await,
            crate::protocol::sd::RebootFlag::RecentlyRebooted
        );

        // Simulate post-wrap state (normally set by `unbind_discovery`
        // reading the departing socket's `reboot_flag`).
        client.force_sd_session_wrapped_for_test(true).await;
        assert_eq!(
            client.reboot_flag().await,
            crate::protocol::sd::RebootFlag::Continuous,
            "reboot_flag must report Continuous from persisted state while \
             discovery is unbound"
        );

        // Rebinding with persisted wrap state seeds the socket via
        // `bind_discovery_seeded`, so the live flag agrees.
        client.bind_discovery().await.unwrap();
        assert_eq!(
            client.reboot_flag().await,
            crate::protocol::sd::RebootFlag::Continuous,
            "seeded socket must report Continuous after wrapped rebind"
        );

        client.shut_down();
    }

    #[tokio::test]
    async fn test_reboot_flag_defaults_to_recently_rebooted() {
        let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
        tokio::spawn(run_fut);
        // Discovery not bound — should fall back to RecentlyRebooted.
        assert_eq!(
            client.reboot_flag().await,
            crate::protocol::sd::RebootFlag::RecentlyRebooted
        );
        client.bind_discovery().await.unwrap();
        // Freshly bound socket also reports RecentlyRebooted (session has not wrapped).
        assert_eq!(
            client.reboot_flag().await,
            crate::protocol::sd::RebootFlag::RecentlyRebooted
        );
        client.shut_down();
    }

    #[tokio::test]
    async fn test_start_sd_announcements_stops_on_shutdown() {
        let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
        tokio::spawn(run_fut);
        client.bind_discovery().await.unwrap();

        let sd_header = empty_sd_header();
        let handle =
            client.start_sd_announcements(sd_header, std::time::Duration::from_millis(100));

        // Shut down the client — the weak sender should fail to upgrade
        // and the task should exit cleanly without needing abort().
        client.shut_down();

        let join_result = tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("task should have exited within timeout");
        // Verify clean exit — not a panic
        assert!(
            join_result.is_ok() || join_result.as_ref().unwrap_err().is_cancelled(),
            "task should have exited cleanly, not panicked"
        );
    }

    /// Documents the footgun: if the caller drops `run_fut` without ever
    /// polling it, the control channel's receiver goes with it and
    /// subsequent `Client` method calls return [`Error::Shutdown`]
    /// rather than panicking.
    ///
    /// This is intrinsic to the caller-driven lifecycle introduced in
    /// phase 6 — the run loop is no longer owned by `Client::new`, so
    /// failing to spawn it is the caller's responsibility. The test
    /// pins the behavior deterministically so that any attempt to
    /// silently "fix" this (e.g. internal spawn fallback) would break
    /// it and force a review.
    ///
    /// Prior to the phase-6 API change these call sites panicked on
    /// `.unwrap()` of the send `Result`; the typed error surfaced here
    /// lets library consumers observe lifecycle mismatches cleanly
    /// instead of bringing down the caller's task.
    #[tokio::test]
    async fn dropping_run_future_without_spawn_returns_shutdown_error() {
        let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
        // Caller explicitly discards the run loop.
        drop(run_fut);
        let err = client
            .bind_discovery()
            .await
            .expect_err("must surface a typed error, not Ok or panic");
        assert!(
            matches!(err, Error::Shutdown),
            "expected Error::Shutdown after run-loop drop, got {err:?}",
        );
    }

    /// If the run loop is cancelled mid-poll (caller-initiated timeout,
    /// graceful shutdown), subsequent `Client` calls see the control
    /// channel closed and surface [`Error::Shutdown`]. Same structural
    /// contract as dropping the run future.
    #[tokio::test]
    async fn cancelling_run_future_closes_control_channel_returns_shutdown_error() {
        let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
        let handle = tokio::spawn(run_fut);
        // Let the loop start.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        handle.abort();
        // Give the abort time to land.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let err = client
            .bind_discovery()
            .await
            .expect_err("must surface a typed error, not Ok or panic");
        assert!(
            matches!(err, Error::Shutdown),
            "expected Error::Shutdown after run-loop cancel, got {err:?}",
        );
    }
}
