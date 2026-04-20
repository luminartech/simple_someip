//! SOME/IP Server/Provider functionality
//!
//! This module provides server-side SOME/IP functionality including:
//! - Service offering/announcement via Service Discovery
//! - Event publishing to subscribers
//! - Event group management
//! - Request/Response handling

mod error;
mod event_publisher;
mod service_info;
mod subscription_manager;

pub use error::Error;
pub use event_publisher::EventPublisher;
pub use service_info::{EventGroupInfo, ServiceInfo};
pub use subscription_manager::SubscriptionManager;

use crate::e2e::{E2EKey, E2EProfile, E2ERegistry};
use crate::protocol::sd::{self, Entry, Flags, OptionsCount, ServiceEntry, TransportProtocol};
use core::sync::atomic::Ordering;
use std::{
    format,
    net::{IpAddr, Ipv4Addr, SocketAddrV4},
    sync::{Arc, Mutex, atomic::AtomicU16},
    vec,
    vec::Vec,
};
use tokio::{net::UdpSocket, sync::RwLock};

/// Configuration for a SOME/IP service provider
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Local interface IP address
    pub interface: Ipv4Addr,
    /// Port to bind for receiving subscriptions and requests
    pub local_port: u16,
    /// Service ID being offered
    pub service_id: u16,
    /// Instance ID
    pub instance_id: u16,
    /// Major version
    pub major_version: u8,
    /// Minor version
    pub minor_version: u32,
    /// Service Discovery TTL (time to live)
    pub ttl: u32,
}

impl ServerConfig {
    /// Create a new server configuration
    #[must_use]
    pub fn new(interface: Ipv4Addr, local_port: u16, service_id: u16, instance_id: u16) -> Self {
        Self {
            interface,
            local_port,
            service_id,
            instance_id,
            major_version: 1,
            minor_version: 0,
            ttl: 3, // 3 seconds is typical for SOME/IP
        }
    }
}

/// SOME/IP Server that can offer services and publish events
pub struct Server {
    config: ServerConfig,
    /// Socket for receiving subscription requests
    unicast_socket: Arc<UdpSocket>,
    /// Socket for sending SD announcements
    sd_socket: Arc<UdpSocket>,
    /// Subscription manager
    subscriptions: Arc<RwLock<SubscriptionManager>>,
    /// Event publisher
    publisher: Arc<EventPublisher>,
    /// Incrementing session ID for SD messages
    sd_session_id: Arc<AtomicU16>,
    /// Shared E2E registry for runtime E2E configuration
    e2e_registry: Arc<Mutex<E2ERegistry>>,
    /// `true` if this server was constructed via [`Server::new_passive`].
    /// Passive servers have no real SD socket bound to port 30490; their
    /// SD handling is managed externally. Calling [`Self::start_announcing`]
    /// or [`Self::run`] on a passive server is a programming error and
    /// returns an [`Error::Io`] with [`std::io::ErrorKind::InvalidInput`].
    is_passive: bool,
}

impl Server {
    /// Create a new SOME/IP server
    ///
    /// # Errors
    ///
    /// Returns an error if binding the unicast or SD socket fails, or if joining the
    /// SD multicast group fails.
    pub async fn new(config: ServerConfig) -> Result<Self, Error> {
        Self::new_with_loopback(config, false).await
    }

    /// Like [`Self::new`], but with explicit control over multicast loopback.
    ///
    /// When `multicast_loopback` is `true`, SD messages sent by this server
    /// are looped back to sockets on the same host — including this server's
    /// own SD socket. This is required when running both a server and a
    /// client/simulator on the same machine for testing. Defaults to `false`
    /// in [`Self::new`].
    ///
    /// # Loopback caveat
    ///
    /// With loopback enabled, this server's SD receive loop (see
    /// [`Self::run`]) will observe the `OfferService` announcements it just
    /// sent. [`Self::run`] already ignores SD entry types that are
    /// not `Subscribe` / `SubscribeAck` / `FindService`, so self-sent
    /// offers are harmless. If this server has ever offered its own
    /// service ID, any self-sent `FindService` for that same service would
    /// also be answered with a unicast `OfferService` reply back to itself;
    /// this is expected and symmetric with how an external peer's
    /// `FindService` would be handled.
    ///
    /// # Errors
    ///
    /// Returns an error if binding the unicast or SD socket fails, or if joining the
    /// SD multicast group fails.
    pub async fn new_with_loopback(
        config: ServerConfig,
        multicast_loopback: bool,
    ) -> Result<Self, Error> {
        // Bind unicast socket for receiving subscriptions
        let unicast_addr = SocketAddrV4::new(config.interface, config.local_port);
        let unicast_socket = Arc::new(UdpSocket::bind(unicast_addr).await?);
        tracing::info!(
            "Server bound to {} for service 0x{:04X}",
            unicast_addr,
            config.service_id
        );

        // Bind SD socket for sending/receiving SD messages (must use SD port 30490)
        let expected_sd_port = sd::MULTICAST_PORT;
        let sd_bind_addr =
            std::net::SocketAddr::new(IpAddr::V4(config.interface), expected_sd_port);
        let sd_raw_socket = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        sd_raw_socket.set_reuse_address(true)?;
        #[cfg(unix)]
        sd_raw_socket.set_reuse_port(true)?;
        sd_raw_socket.set_multicast_if_v4(&config.interface)?;
        sd_raw_socket.set_multicast_loop_v4(multicast_loopback)?;
        sd_raw_socket.bind(&sd_bind_addr.into())?;
        sd_raw_socket.set_nonblocking(true)?;
        let sd_std_socket: std::net::UdpSocket = sd_raw_socket.into();
        let sd_socket = UdpSocket::from_std(sd_std_socket)?;

        // Join SD multicast group to receive FindService and SubscribeEventGroup
        sd_socket.join_multicast_v4(sd::MULTICAST_IP, config.interface)?;
        let actual_sd_addr = sd_socket.local_addr()?;
        tracing::info!(
            "Server SD socket bound to {} (expected port {}), joined multicast {}",
            actual_sd_addr,
            expected_sd_port,
            sd::MULTICAST_IP
        );
        if let std::net::SocketAddr::V4(v4) = actual_sd_addr
            && v4.port() != expected_sd_port
        {
            tracing::error!(
                "SD socket port mismatch! Expected {}, got {}. Offers will use wrong source port.",
                expected_sd_port,
                v4.port()
            );
        }

        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let e2e_registry = Arc::new(Mutex::new(E2ERegistry::new()));
        let publisher = Arc::new(EventPublisher::new(
            Arc::clone(&subscriptions),
            Arc::clone(&unicast_socket),
            Arc::clone(&e2e_registry),
        ));

        Ok(Self {
            config,
            unicast_socket,
            sd_socket: Arc::new(sd_socket),
            subscriptions,
            publisher,
            sd_session_id: Arc::new(AtomicU16::new(1)),
            e2e_registry,
            is_passive: false,
        })
    }

    /// Create a passive SOME/IP server.
    ///
    /// A passive server binds its unicast socket at `config.local_port` as
    /// usual (so `publish_raw_event` has a real source port matching the
    /// endpoint advertised in external `OfferService` messages), but binds
    /// its SD socket to an ephemeral port instead of the SOME/IP SD port
    /// (30490). The passive server is therefore **not** part of the
    /// `SO_REUSEPORT` group at 30490, and the kernel will never deliver SD
    /// traffic destined for 30490 to it.
    ///
    /// Passive servers are intended for use with an external SD dispatcher
    /// (for example, a `Client` whose discovery socket receives all
    /// incoming `SubscribeEventGroup` / `FindService` messages and routes
    /// them to the right `EventPublisher` via
    /// [`EventPublisher::register_subscriber`]). Do **not** call
    /// [`Server::start_announcing`] or spawn [`Server::run`] on a passive
    /// server — the external dispatcher owns those responsibilities.
    ///
    /// # Errors
    ///
    /// Returns an error if binding either socket fails.
    pub async fn new_passive(config: ServerConfig) -> Result<Self, Error> {
        // Bind unicast socket at the configured local_port — the passive
        // server still needs a real source port so published events appear
        // to come from the endpoint advertised in the external OfferService.
        let unicast_addr = SocketAddrV4::new(config.interface, config.local_port);
        let unicast_socket = Arc::new(UdpSocket::bind(unicast_addr).await?);
        tracing::info!(
            "Passive server bound to {} for service 0x{:04X}",
            unicast_addr,
            config.service_id
        );

        // Bind a placeholder SD socket on an ephemeral port. Nothing will
        // route to it (neither multicast nor unicast on 30490), and neither
        // `start_announcing` nor `run` should be called for a passive
        // server. We still allocate it so the `Server` struct shape is
        // identical to the full-server path.
        let sd_placeholder_addr = std::net::SocketAddr::new(IpAddr::V4(config.interface), 0);
        let sd_socket = UdpSocket::bind(sd_placeholder_addr).await?;
        // Log the bound address using `Debug` on the `Result<SocketAddr>`
        // so a hypothetical `local_addr` failure does not propagate as a
        // construction error and we do not introduce an unreachable Err
        // arm purely for defensive logging.
        tracing::info!(
            "Passive server SD placeholder socket bound to {:?} (not in SD reuseport group)",
            sd_socket.local_addr()
        );

        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let e2e_registry = Arc::new(Mutex::new(E2ERegistry::new()));
        let publisher = Arc::new(EventPublisher::new(
            Arc::clone(&subscriptions),
            Arc::clone(&unicast_socket),
            Arc::clone(&e2e_registry),
        ));

        Ok(Self {
            config,
            unicast_socket,
            sd_socket: Arc::new(sd_socket),
            subscriptions,
            publisher,
            sd_session_id: Arc::new(AtomicU16::new(1)),
            e2e_registry,
            is_passive: true,
        })
    }

    /// Start announcing the service via Service Discovery
    ///
    /// This sends periodic `OfferService` messages to the SD multicast group
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] with [`std::io::ErrorKind::InvalidInput`] if
    /// called on a server constructed via [`Server::new_passive`] — passive
    /// servers have no real SD socket bound to port 30490, so any
    /// announcements would go out with an incorrect source port.
    ///
    /// Otherwise currently always returns `Ok(())`; SD send failures are
    /// logged internally.
    pub fn start_announcing(&self) -> Result<(), Error> {
        if self.is_passive {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "start_announcing called on passive Server for service 0x{:04X}; \
                     announcements must be driven externally (e.g. via \
                     `simple_someip::Client::start_sd_announcements`)",
                    self.config.service_id
                ),
            )));
        }
        let config = self.config.clone();
        let sd_socket = Arc::clone(&self.sd_socket);
        let sd_session_id = Arc::clone(&self.sd_session_id);

        tokio::spawn(async move {
            let mut announcement_count = 0u32;
            loop {
                match Self::send_offer_service(&config, &sd_socket, &sd_session_id).await {
                    Ok(()) => {
                        announcement_count += 1;
                        if announcement_count == 1 {
                            tracing::info!(
                                "Sent first SD announcement for service 0x{:04X}",
                                config.service_id
                            );
                        } else {
                            tracing::debug!(
                                "Sent {} SD announcements for service 0x{:04X}",
                                announcement_count,
                                config.service_id
                            );
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to send OfferService: {:?}", e);
                    }
                }

                // Send announcements every 1 second
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            }
        });

        Ok(())
    }

    /// Send an `OfferService` message via Service Discovery
    async fn send_offer_service(
        config: &ServerConfig,
        socket: &UdpSocket,
        session_id: &AtomicU16,
    ) -> Result<(), Error> {
        use crate::protocol::Header as SomeIpHeader;
        use crate::traits::WireFormat;

        // Create OfferService entry
        let entry = Entry::OfferService(ServiceEntry {
            index_first_options_run: 0,
            index_second_options_run: 0,
            options_count: OptionsCount::new(1, 0),
            service_id: config.service_id,
            instance_id: config.instance_id,
            major_version: config.major_version,
            ttl: config.ttl,
            minor_version: config.minor_version,
        });

        // Create IPv4 endpoint option
        let option = sd::Options::IpV4Endpoint {
            ip: config.interface,
            port: config.local_port,
            protocol: TransportProtocol::Udp,
        };

        let entries = [entry];
        let options = [option];
        let sd_payload = sd::Header::new(Flags::new(true, true), &entries, &options);

        // Encode SD payload
        let mut sd_data = Vec::new();
        sd_payload.encode(&mut sd_data)?;

        // Increment session ID (wrapping from 0xFFFF back to 0x0001, skipping 0)
        let prev = session_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                let next = v.wrapping_add(1);
                Some(if next == 0 { 1 } else { next })
            })
            .unwrap();
        let next = prev.wrapping_add(1);
        let sid = u32::from(if next == 0 { 1 } else { next });

        // Wrap in SOME/IP header for SD (service 0xFFFF, method 0x8100)
        let someip_header = SomeIpHeader::new_sd(sid, sd_data.len());

        // Encode complete SOME/IP-SD message
        let mut buffer = Vec::new();
        someip_header.encode(&mut buffer)?;
        buffer.extend_from_slice(&sd_data);

        let multicast_addr = SocketAddrV4::new(sd::MULTICAST_IP, sd::MULTICAST_PORT);

        tracing::trace!(
            "Sending OfferService: service=0x{:04X}, instance={}, port={}, size={} bytes",
            config.service_id,
            config.instance_id,
            config.local_port,
            buffer.len()
        );
        tracing::trace!(
            "OfferService data: {:02X?}",
            &buffer[..buffer.len().min(64)]
        );

        socket.send_to(&buffer, multicast_addr).await?;
        tracing::trace!("Sent to {}", multicast_addr);

        Ok(())
    }

    /// Send a unicast `OfferService` to a specific address (in response to `FindService`)
    async fn send_unicast_offer(&self, target: std::net::SocketAddr) -> Result<(), Error> {
        use crate::protocol::Header as SomeIpHeader;
        use crate::traits::WireFormat;

        let entry = Entry::OfferService(ServiceEntry {
            index_first_options_run: 0,
            index_second_options_run: 0,
            options_count: OptionsCount::new(1, 0),
            service_id: self.config.service_id,
            instance_id: self.config.instance_id,
            major_version: self.config.major_version,
            ttl: self.config.ttl,
            minor_version: self.config.minor_version,
        });

        let option = sd::Options::IpV4Endpoint {
            ip: self.config.interface,
            port: self.config.local_port,
            protocol: TransportProtocol::Udp,
        };

        let entries = [entry];
        let options = [option];
        let sd_payload = sd::Header::new(Flags::new(true, true), &entries, &options);

        let mut sd_data = Vec::new();
        sd_payload.encode(&mut sd_data)?;

        let sid = self.next_sd_session_id();
        let someip_header = SomeIpHeader::new_sd(sid, sd_data.len());

        let mut buffer = Vec::new();
        someip_header.encode(&mut buffer)?;
        buffer.extend_from_slice(&sd_data);

        self.sd_socket.send_to(&buffer, target).await?;
        tracing::debug!(
            "Sent unicast OfferService to {} for service 0x{:04X}",
            target,
            self.config.service_id
        );

        Ok(())
    }

    /// Get the next SD session ID (`client_id=0`, `session_id` incrementing), skipping 0
    fn next_sd_session_id(&self) -> u32 {
        let prev = self
            .sd_session_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                let next = v.wrapping_add(1);
                Some(if next == 0 { 1 } else { next })
            })
            .unwrap();
        // fetch_update returns the previous value; compute the same next value
        let next = prev.wrapping_add(1);
        u32::from(if next == 0 { 1 } else { next })
    }

    /// Get the event publisher for sending events
    #[must_use]
    pub fn publisher(&self) -> Arc<EventPublisher> {
        Arc::clone(&self.publisher)
    }

    /// Get the local address of the unicast socket.
    ///
    /// # Errors
    ///
    /// Returns an error if the socket's local address cannot be retrieved.
    pub fn unicast_local_addr(&self) -> Result<std::net::SocketAddr, std::io::Error> {
        self.unicast_socket.local_addr()
    }

    /// Update the configured local port (useful after binding to ephemeral port 0).
    pub fn set_local_port(&mut self, port: u16) {
        self.config.local_port = port;
    }

    /// Register an E2E profile for the given key.
    ///
    /// Once registered, outgoing events published via [`EventPublisher::publish_event`]
    /// will have E2E protection applied automatically.
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

    /// Run the server event loop
    ///
    /// Handles incoming subscription requests and manages event groups.
    /// Listens on both the unicast socket (for direct requests) and the
    /// SD multicast socket (for `FindService` and `SubscribeEventGroup`).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] with [`std::io::ErrorKind::InvalidInput`] if
    /// called on a server constructed via [`Server::new_passive`] — passive
    /// servers have no real SD socket to read from, so the run loop would
    /// block forever on the ephemeral placeholder socket.
    ///
    /// Otherwise returns an error if receiving from a socket fails or
    /// handling an SD message fails.
    pub async fn run(&mut self) -> Result<(), Error> {
        use crate::protocol::MessageView;

        if self.is_passive {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "run called on passive Server for service 0x{:04X}; \
                     SD receive must be driven externally (e.g. via the \
                     Client's discovery socket, routing Subscribes to \
                     `EventPublisher::register_subscriber`)",
                    self.config.service_id
                ),
            )));
        }

        let mut unicast_buf = vec![0u8; 65535];
        let mut sd_buf = vec![0u8; 65535];

        loop {
            let (data, len, addr, source) = tokio::select! {
                result = self.unicast_socket.recv_from(&mut unicast_buf) => {
                    let (len, addr) = result?;
                    (&unicast_buf[..], len, addr, "unicast")
                }
                result = self.sd_socket.recv_from(&mut sd_buf) => {
                    let (len, addr) = result?;
                    (&sd_buf[..], len, addr, "sd-multicast")
                }
            };
            let data = &data[..len];

            // By default IP_MULTICAST_LOOP=false suppresses own multicast
            // messages on the SD socket, so no source-IP filtering is needed.
            // When the server was constructed via `Server::new_with_loopback`
            // with `multicast_loopback = true` (e.g. for same-host testing),
            // the kernel delivers our own SD multicasts back to this loop.
            // That is tolerated here: `handle_sd_message` only acts on
            // `Subscribe` / `SubscribeAck` / `FindService` entries, so the
            // `OfferService` entries we send ourselves are effectively
            // ignored. A self-sent `FindService` for our own service ID
            // would trigger a unicast `OfferService` reply back to
            // ourselves, which is the same behavior an external peer's
            // `FindService` would produce and is therefore safe.

            tracing::trace!("Received {} bytes from {} on {} socket", len, addr, source);
            tracing::trace!("Raw data: {:02X?}", &data[..len.min(64_usize)]);

            // Try to parse as SOME/IP message using zero-copy view
            match MessageView::parse(data) {
                Ok(view) => {
                    tracing::trace!(
                        "SOME/IP Header: service=0x{:04X}, method=0x{:04X}, type={:?}",
                        view.header().message_id().service_id(),
                        view.header().message_id().method_id(),
                        view.header().message_type().message_type()
                    );

                    // Check if this is a Service Discovery message (0xFFFF8100)
                    if view.is_sd() {
                        tracing::trace!("This is an SD message");
                        // Parse SD payload
                        match view.sd_header() {
                            Ok(sd_view) => {
                                tracing::trace!("SD message has {} entries", sd_view.entry_count(),);
                                self.handle_sd_message(&sd_view, addr).await?;
                            }
                            Err(e) => {
                                tracing::warn!("Failed to parse SD message: {:?}", e);
                            }
                        }
                    } else {
                        tracing::trace!("Non-SD SOME/IP message, ignoring");
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to parse SOME/IP header from {}: {:?}", addr, e);
                    tracing::trace!("Data: {:02X?}", &data[..len.min(32)]);
                }
            }
        }
    }

    /// Handle a Service Discovery message
    async fn handle_sd_message(
        &mut self,
        sd_view: &sd::SdHeaderView<'_>,
        sender: std::net::SocketAddr,
    ) -> Result<(), Error> {
        tracing::trace!("Handling SD message from {}", sender);

        for entry_view in sd_view.entries() {
            let entry_type = entry_view.entry_type()?;
            match entry_type {
                sd::EntryType::Subscribe => {
                    tracing::debug!(
                        "Received Subscribe from {}: service=0x{:04X}, instance={}, eventgroup=0x{:04X}",
                        sender,
                        entry_view.service_id(),
                        entry_view.instance_id(),
                        entry_view.event_group_id()
                    );

                    // Check if this is for our service.
                    if entry_view.service_id() != self.config.service_id {
                        tracing::warn!(
                            "Subscribe for wrong service: expected 0x{:04X}, got 0x{:04X}",
                            self.config.service_id,
                            entry_view.service_id()
                        );
                        self.send_subscribe_nack_from_view(&entry_view, sender, "Wrong service ID")
                            .await?;
                    } else if entry_view.instance_id() != self.config.instance_id {
                        tracing::warn!(
                            "Subscribe for wrong instance: expected {}, got {}",
                            self.config.instance_id,
                            entry_view.instance_id()
                        );
                        self.send_subscribe_nack_from_view(
                            &entry_view,
                            sender,
                            "Wrong instance ID",
                        )
                        .await?;
                    } else {
                        // Extract the subscriber endpoint from the entry's
                        // own options run. Each SD entry describes two runs
                        // of options via `(index_first_options_run,
                        // first_options_count)` and the symmetric second
                        // pair; we walk both runs, collect every
                        // `IpV4Endpoint` option in them, and take the first.
                        let first_index = entry_view.index_first_options_run() as usize;
                        let first_count = entry_view.options_count().first_options_count as usize;
                        let second_index = entry_view.index_second_options_run() as usize;
                        let second_count = entry_view.options_count().second_options_count as usize;
                        if let Some(endpoint_addr) = Self::extract_subscriber_endpoint(
                            &sd_view.options(),
                            first_index,
                            first_count,
                            second_index,
                            second_count,
                        ) {
                            let mut subs = self.subscriptions.write().await;
                            subs.subscribe(
                                entry_view.service_id(),
                                entry_view.instance_id(),
                                entry_view.event_group_id(),
                                endpoint_addr,
                            );

                            // Send SubscribeAck
                            self.send_subscribe_ack_from_view(&entry_view, sender)
                                .await?;
                        } else {
                            tracing::warn!("No endpoint found in Subscribe message options");
                            self.send_subscribe_nack_from_view(
                                &entry_view,
                                sender,
                                "No endpoint in options",
                            )
                            .await?;
                        }
                    }
                }
                sd::EntryType::FindService => {
                    let find_service_id = entry_view.service_id();
                    // Check if this FindService is for our service (or wildcard 0xFFFF)
                    if find_service_id == self.config.service_id || find_service_id == 0xFFFF {
                        tracing::debug!(
                            "Received FindService from {} for service 0x{:04X} (ours: 0x{:04X}), sending unicast offer",
                            sender,
                            find_service_id,
                            self.config.service_id
                        );
                        self.send_unicast_offer(sender).await?;
                    } else {
                        tracing::trace!(
                            "Ignoring FindService for service 0x{:04X} (not ours)",
                            find_service_id
                        );
                    }
                }
                _ => {
                    tracing::trace!("Ignoring SD entry type: {:?}", entry_type);
                }
            }
        }

        Ok(())
    }

    /// Extract a single subscriber endpoint from the options runs
    /// associated with an SD entry.
    ///
    /// Each SD entry owns up to two options runs. A run is a contiguous
    /// slice of the options array starting at `index_*_options_run` with
    /// `*_options_count` entries. This helper walks both runs, collects
    /// every `IpV4Endpoint` option it finds, returns the first, and logs
    /// a `warn!` if more than one endpoint is present (we do not yet
    /// support multi-endpoint subscribers — e.g. TCP+UDP — and will pick
    /// an arbitrary one).
    ///
    /// Returns `None` if no `IpV4Endpoint` is found in either run.
    fn extract_subscriber_endpoint(
        options: &sd::OptionIter<'_>,
        first_index: usize,
        first_count: usize,
        second_index: usize,
        second_count: usize,
    ) -> Option<SocketAddrV4> {
        // Walk each run by cloning the iterator — `OptionIter` is a
        // cheap view over borrowed bytes so `clone` is free. Taking
        // `options` by reference lets the caller keep ownership and
        // keeps the clippy `needless_pass_by_value` lint quiet.
        //
        // We only ever return the first `IpV4Endpoint` found, so rather
        // than collect into a `Vec` (heap alloc on every Subscribe) we
        // track the first hit in an `Option` and keep a count so the
        // multi-endpoint warn path still reports how many additional
        // endpoints were present. This keeps the SD receive loop
        // allocation-free on the happy path.
        let mut first_endpoint: Option<SocketAddrV4> = None;
        let mut endpoint_count: usize = 0;
        let mut ignored_other: usize = 0;

        let mut walk_run = |index: usize, count: usize| {
            if count == 0 {
                return;
            }
            for option_view in options.clone().skip(index).take(count) {
                match option_view.option_type() {
                    Ok(sd::OptionType::IpV4Endpoint) => {
                        if let Ok((ip, _, port)) = option_view.as_ipv4() {
                            endpoint_count += 1;
                            if first_endpoint.is_none() {
                                first_endpoint = Some(SocketAddrV4::new(ip, port));
                            }
                        }
                    }
                    Ok(_) | Err(_) => ignored_other += 1,
                }
            }
        };

        walk_run(first_index, first_count);
        walk_run(second_index, second_count);

        match endpoint_count {
            0 => {
                tracing::warn!(
                    "No IPv4 endpoint in options runs \
                     (first: idx={first_index}, count={first_count}; \
                     second: idx={second_index}, count={second_count}; \
                     ignored={ignored_other})"
                );
                None
            }
            1 => {
                // Unwrap is safe: count == 1 implies we set `first_endpoint`.
                let ep = first_endpoint.expect("endpoint_count=1 implies first_endpoint is Some");
                tracing::trace!("Found IPv4 endpoint {}", ep);
                Some(ep)
            }
            n => {
                let ep = first_endpoint.expect("endpoint_count>=1 implies first_endpoint is Some");
                tracing::warn!(
                    "{} IPv4 endpoints found in subscribe options runs; \
                     using first ({}) and ignoring {} additional. \
                     Multi-endpoint (e.g. TCP+UDP) subscribers are not yet supported.",
                    n,
                    ep,
                    n - 1
                );
                Some(ep)
            }
        }
    }

    /// Send `SubscribeAck` from an entry view
    async fn send_subscribe_ack_from_view(
        &self,
        entry_view: &sd::EntryView<'_>,
        subscriber: std::net::SocketAddr,
    ) -> Result<(), Error> {
        use crate::protocol::Header as SomeIpHeader;
        use crate::traits::WireFormat;

        let ack_entry = Entry::SubscribeAckEventGroup(sd::EventGroupEntry {
            index_first_options_run: 0,
            index_second_options_run: 0,
            options_count: OptionsCount::new(0, 0),
            service_id: entry_view.service_id(),
            instance_id: entry_view.instance_id(),
            major_version: entry_view.major_version(),
            ttl: self.config.ttl,
            counter: entry_view.counter(),
            event_group_id: entry_view.event_group_id(),
        });

        let entries = [ack_entry];
        let sd_payload = sd::Header::new(Flags::new(true, true), &entries, &[]);

        let mut sd_data = Vec::new();
        sd_payload.encode(&mut sd_data)?;

        let sid = self.next_sd_session_id();
        let someip_header = SomeIpHeader::new_sd(sid, sd_data.len());

        let mut buffer = Vec::new();
        someip_header.encode(&mut buffer)?;
        buffer.extend_from_slice(&sd_data);

        self.sd_socket.send_to(&buffer, subscriber).await?;

        tracing::debug!(
            "Sent SubscribeAck to {} for service 0x{:04X}, eventgroup 0x{:04X}",
            subscriber,
            entry_view.service_id(),
            entry_view.event_group_id()
        );

        Ok(())
    }

    /// Send `SubscribeNack` from an entry view
    async fn send_subscribe_nack_from_view(
        &self,
        entry_view: &sd::EntryView<'_>,
        subscriber: std::net::SocketAddr,
        reason: &str,
    ) -> Result<(), Error> {
        use crate::protocol::Header as SomeIpHeader;
        use crate::traits::WireFormat;

        let nack_entry = Entry::SubscribeAckEventGroup(sd::EventGroupEntry {
            index_first_options_run: 0,
            index_second_options_run: 0,
            options_count: OptionsCount::new(0, 0),
            service_id: entry_view.service_id(),
            instance_id: entry_view.instance_id(),
            major_version: entry_view.major_version(),
            ttl: 0, // TTL=0 indicates NACK
            counter: entry_view.counter(),
            event_group_id: entry_view.event_group_id(),
        });

        let entries = [nack_entry];
        let sd_payload = sd::Header::new(Flags::new(true, true), &entries, &[]);

        let mut sd_data = Vec::new();
        sd_payload.encode(&mut sd_data)?;

        let sid = self.next_sd_session_id();
        let someip_header = SomeIpHeader::new_sd(sid, sd_data.len());

        let mut buffer = Vec::new();
        someip_header.encode(&mut buffer)?;
        buffer.extend_from_slice(&sd_data);

        self.sd_socket.send_to(&buffer, subscriber).await?;

        tracing::warn!(
            "Sent SubscribeNack to {} for service 0x{:04X}, eventgroup 0x{:04X} (reason: {})",
            subscriber,
            entry_view.service_id(),
            entry_view.event_group_id(),
            reason
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        Header as SomeIpHeader, MessageType, MessageTypeField, MessageView, ReturnCode,
    };
    use crate::traits::WireFormat;
    use std::format;

    #[tokio::test]
    async fn test_server_creation() {
        let config = ServerConfig::new(Ipv4Addr::new(127, 0, 0, 1), 30682, 0x5B, 1);

        let server: Result<Server, _> = Server::new(config).await;
        assert!(server.is_ok());
    }

    #[tokio::test]
    async fn test_server_creation_with_loopback_enabled() {
        // Use a unicast port distinct from other tests to avoid EADDRINUSE
        // when the test binary runs tests in parallel. The SD socket binds
        // the SD multicast port (30490) and relies on SO_REUSEPORT, the same
        // as `test_server_creation`.
        let config = ServerConfig::new(Ipv4Addr::new(127, 0, 0, 1), 30683, 0x5C, 1);

        let server = Server::new_with_loopback(config, true)
            .await
            .expect("new_with_loopback(true) should succeed on localhost");

        // Confirm the SD socket was actually configured with IP_MULTICAST_LOOP
        // enabled — this is the behavior the new code path is supposed to
        // produce and is what makes same-host testing possible.
        let sock_ref = socket2::SockRef::from(&*server.sd_socket);
        assert!(
            sock_ref
                .multicast_loop_v4()
                .expect("multicast_loop_v4 getter should succeed"),
            "multicast loopback should be enabled on the SD socket",
        );
    }

    /// Helper: wrap an SD header in a SOME/IP SD message and return the bytes
    fn build_sd_message(sd_header: &sd::Header<'_>) -> Vec<u8> {
        let mut sd_data = Vec::new();
        sd_header.encode(&mut sd_data).unwrap();

        let someip_header = SomeIpHeader::new_sd(0x0001, sd_data.len());

        let mut buffer = Vec::new();
        someip_header.encode(&mut buffer).unwrap();
        buffer.extend_from_slice(&sd_data);
        buffer
    }

    /// Helper: parse a SubscribeAck/Nack from raw response bytes, returns the TTL
    fn parse_subscribe_ack_ttl(data: &[u8]) -> u32 {
        let view = MessageView::parse(data).expect("Failed to parse SOME/IP message");
        let sd_view = view.sd_header().expect("Failed to parse SD header");
        let mut entries = sd_view.entries();
        let entry = entries.next().expect("Expected at least 1 entry");
        assert_eq!(
            entry.entry_type().unwrap(),
            sd::EntryType::SubscribeAck,
            "Expected SubscribeAckEventGroup entry"
        );
        entry.ttl()
    }

    /// Helper: create a server on an ephemeral port and return (Server, port)
    async fn create_test_server(service_id: u16, instance_id: u16) -> (Server, u16) {
        // Use port 0 to get an ephemeral port
        let config = ServerConfig::new(Ipv4Addr::new(127, 0, 0, 1), 0, service_id, instance_id);
        let mut server = Server::new(config).await.expect("Failed to create server");
        let port = match server.unicast_local_addr().unwrap() {
            std::net::SocketAddr::V4(addr) => addr.port(),
            _ => panic!("Expected IPv4 address"),
        };
        // Update config to reflect actual bound port
        server.set_local_port(port);
        (server, port)
    }

    fn make_subscription_header(
        service_id: u16,
        instance_id: u16,
        major_version: u8,
        ttl: u32,
        event_group_id: u16,
        client_ip: Ipv4Addr,
        protocol: sd::TransportProtocol,
        client_port: u16,
    ) -> Vec<u8> {
        let entry = Entry::SubscribeEventGroup(sd::EventGroupEntry::new(
            service_id,
            instance_id,
            major_version,
            ttl,
            event_group_id,
        ));
        let endpoint = sd::Options::IpV4Endpoint {
            ip: client_ip,
            protocol,
            port: client_port,
        };
        let entries = [entry];
        let options = [endpoint];
        let sd_header = sd::Header::new(
            Flags::new_sd(sd::RebootFlag::Continuous),
            &entries,
            &options,
        );
        build_sd_message(&sd_header)
    }

    #[tokio::test]
    async fn test_subscribe_ack_success() {
        let (mut server, server_port) = create_test_server(0x5B, 1).await;

        // Create a client socket to send subscription and receive response
        let client_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let message = make_subscription_header(
            0x5B,
            1,
            1,
            3,
            0x01,
            Ipv4Addr::new(127, 0, 0, 1),
            sd::TransportProtocol::Udp,
            server_port,
        );

        // Send to the server
        client_socket
            .send_to(&message, format!("127.0.0.1:{}", server_port))
            .await
            .unwrap();

        // Run server to process one message (with a timeout)
        let server_handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let (len, addr) = server.unicast_socket.recv_from(&mut buf).await.unwrap();
            let data = &buf[..len];
            let view = MessageView::parse(data).unwrap();
            let sd_view = view.sd_header().unwrap();
            server.handle_sd_message(&sd_view, addr).await.unwrap();

            // Check subscription was added
            let subs = server.subscriptions.read().await;
            assert_eq!(subs.subscription_count(), 1);
            let subscribers = subs.get_subscribers(0x5B, 1, 0x01);
            assert_eq!(subscribers.len(), 1);
        });

        // Receive the ACK response
        let mut resp_buf = vec![0u8; 65535];
        let (resp_len, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client_socket.recv_from(&mut resp_buf),
        )
        .await
        .expect("Timeout waiting for SubscribeAck")
        .unwrap();

        let ttl = parse_subscribe_ack_ttl(&resp_buf[..resp_len]);
        assert!(ttl > 0, "Expected ACK (TTL > 0), got TTL={}", ttl);

        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_subscribe_nack_wrong_service() {
        let (mut server, server_port) = create_test_server(0x5B, 1).await;
        let client_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let message = make_subscription_header(
            0x99, // Wrong service
            1,
            1,
            3,
            0x01,
            Ipv4Addr::new(127, 0, 0, 1),
            sd::TransportProtocol::Udp,
            server_port,
        );
        client_socket
            .send_to(&message, format!("127.0.0.1:{}", server_port))
            .await
            .unwrap();

        // Process the message
        let server_handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let (len, addr) = server.unicast_socket.recv_from(&mut buf).await.unwrap();
            let data = &buf[..len];
            let view = MessageView::parse(data).unwrap();
            let sd_view = view.sd_header().unwrap();
            server.handle_sd_message(&sd_view, addr).await.unwrap();

            // No subscription should have been added
            let subs = server.subscriptions.read().await;
            assert_eq!(subs.subscription_count(), 0);
        });

        // Receive the NACK response
        let mut resp_buf = vec![0u8; 65535];
        let (resp_len, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client_socket.recv_from(&mut resp_buf),
        )
        .await
        .expect("Timeout waiting for SubscribeNack")
        .unwrap();

        let ttl = parse_subscribe_ack_ttl(&resp_buf[..resp_len]);
        assert_eq!(ttl, 0, "Expected NACK (TTL=0), got TTL={}", ttl);

        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_subscribe_nack_wrong_instance() {
        let (mut server, server_port) = create_test_server(0x5B, 1).await;
        let client_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let message = make_subscription_header(
            0x5B,
            99, // Wrong instance
            1,
            3,
            0x01,
            Ipv4Addr::new(127, 0, 0, 1),
            sd::TransportProtocol::Udp,
            server_port,
        );
        client_socket
            .send_to(&message, format!("127.0.0.1:{}", server_port))
            .await
            .unwrap();

        let server_handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let (len, addr) = server.unicast_socket.recv_from(&mut buf).await.unwrap();
            let data = &buf[..len];
            let view = MessageView::parse(data).unwrap();
            let sd_view = view.sd_header().unwrap();
            server.handle_sd_message(&sd_view, addr).await.unwrap();

            let subs = server.subscriptions.read().await;
            assert_eq!(subs.subscription_count(), 0);
        });

        let mut resp_buf = vec![0u8; 65535];
        let (resp_len, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client_socket.recv_from(&mut resp_buf),
        )
        .await
        .expect("Timeout waiting for SubscribeNack")
        .unwrap();

        let ttl = parse_subscribe_ack_ttl(&resp_buf[..resp_len]);
        assert_eq!(ttl, 0, "Expected NACK (TTL=0), got TTL={}", ttl);

        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_find_service_sends_unicast_offer() {
        let (mut server, server_port) = create_test_server(0x5B, 1).await;
        let client_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        // Send a FindService for 0x5B
        let find_entry = Entry::FindService(ServiceEntry::find(0x5B));
        let find_entries = [find_entry];
        let sd_header = sd::Header::new(
            Flags::new_sd(sd::RebootFlag::Continuous),
            &find_entries,
            &[],
        );
        let message = build_sd_message(&sd_header);
        client_socket
            .send_to(&message, format!("127.0.0.1:{}", server_port))
            .await
            .unwrap();

        // Process the message on the unicast socket
        let server_handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let (len, addr) = server.unicast_socket.recv_from(&mut buf).await.unwrap();
            let data = &buf[..len];
            let view = MessageView::parse(data).unwrap();
            let sd_view = view.sd_header().unwrap();
            server.handle_sd_message(&sd_view, addr).await.unwrap();
        });

        // Receive the unicast OfferService response
        let mut resp_buf = vec![0u8; 65535];
        let (resp_len, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client_socket.recv_from(&mut resp_buf),
        )
        .await
        .expect("Timeout waiting for unicast OfferService")
        .unwrap();

        // Parse the response and verify it's an OfferService for 0x5B
        let view = MessageView::parse(&resp_buf[..resp_len]).unwrap();
        assert_eq!(view.header().message_id().service_id(), 0xFFFF);
        let sd_view = view.sd_header().unwrap();
        let mut entries = sd_view.entries();
        let entry = entries.next().unwrap();
        assert_eq!(entry.entry_type().unwrap(), sd::EntryType::OfferService);
        assert_eq!(entry.service_id(), 0x5B);

        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_find_service_wildcard() {
        let (mut server, server_port) = create_test_server(0x5B, 1).await;
        let client_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        // Send wildcard FindService (0xFFFF)
        let find_entry = Entry::FindService(ServiceEntry::find(0xFFFF));
        let find_entries = [find_entry];
        let sd_header = sd::Header::new(
            Flags::new_sd(sd::RebootFlag::Continuous),
            &find_entries,
            &[],
        );
        let message = build_sd_message(&sd_header);
        client_socket
            .send_to(&message, format!("127.0.0.1:{}", server_port))
            .await
            .unwrap();

        let server_handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let (len, addr) = server.unicast_socket.recv_from(&mut buf).await.unwrap();
            let data = &buf[..len];
            let view = MessageView::parse(data).unwrap();
            let sd_view = view.sd_header().unwrap();
            server.handle_sd_message(&sd_view, addr).await.unwrap();
        });

        let mut resp_buf = vec![0u8; 65535];
        let (resp_len, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client_socket.recv_from(&mut resp_buf),
        )
        .await
        .expect("Timeout waiting for unicast OfferService")
        .unwrap();

        let view = MessageView::parse(&resp_buf[..resp_len]).unwrap();
        let sd_view = view.sd_header().unwrap();
        let mut entries = sd_view.entries();
        let entry = entries.next().unwrap();
        assert_eq!(entry.entry_type().unwrap(), sd::EntryType::OfferService);
        assert_eq!(entry.service_id(), 0x5B);

        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_find_service_wrong_service_ignored() {
        let (mut server, server_port) = create_test_server(0x5B, 1).await;
        let client_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        // Send FindService for 0x99 (not our service)
        let find_entry = Entry::FindService(ServiceEntry::find(0x99));
        let find_entries = [find_entry];
        let sd_header = sd::Header::new(
            Flags::new_sd(sd::RebootFlag::Continuous),
            &find_entries,
            &[],
        );
        let message = build_sd_message(&sd_header);
        client_socket
            .send_to(&message, format!("127.0.0.1:{}", server_port))
            .await
            .unwrap();

        let server_handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let (len, addr) = server.unicast_socket.recv_from(&mut buf).await.unwrap();
            let data = &buf[..len];
            let view = MessageView::parse(data).unwrap();
            let sd_view = view.sd_header().unwrap();
            server.handle_sd_message(&sd_view, addr).await.unwrap();
        });

        // Should NOT receive any response (short timeout)
        let mut resp_buf = vec![0u8; 65535];
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            client_socket.recv_from(&mut resp_buf),
        )
        .await;
        assert!(
            result.is_err(),
            "Expected timeout (no response for wrong service)"
        );

        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_subscribe_nack_no_endpoint() {
        let (mut server, server_port) = create_test_server(0x5B, 1).await;
        let client_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        // Build a SubscribeEventGroup with NO endpoint option
        let entry = sd::Entry::SubscribeEventGroup(sd::EventGroupEntry::new(0x5B, 1, 1, 3, 0x01));
        let sub_entries = [entry];
        let sd_header = sd::Header::new(Flags::new(true, true), &sub_entries, &[]);
        let message = build_sd_message(&sd_header);

        client_socket
            .send_to(&message, format!("127.0.0.1:{}", server_port))
            .await
            .unwrap();

        let server_handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let (len, addr) = server.unicast_socket.recv_from(&mut buf).await.unwrap();
            let data = &buf[..len];
            let view = MessageView::parse(data).unwrap();
            let sd_view = view.sd_header().unwrap();
            server.handle_sd_message(&sd_view, addr).await.unwrap();

            // No subscription should have been added
            let subs = server.subscriptions.read().await;
            assert_eq!(subs.subscription_count(), 0);
        });

        // Should receive a NACK (TTL=0)
        let mut resp_buf = vec![0u8; 65535];
        let (resp_len, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client_socket.recv_from(&mut resp_buf),
        )
        .await
        .expect("Timeout waiting for SubscribeNack")
        .unwrap();

        let ttl = parse_subscribe_ack_ttl(&resp_buf[..resp_len]);
        assert_eq!(ttl, 0, "Expected NACK (TTL=0), got TTL={}", ttl);

        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_send_offer_service() {
        // Test send_unicast_offer directly (sends to a specific target).
        // send_offer_service sends to multicast which is unreliable on loopback.
        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let recv_addr = receiver.local_addr().unwrap();

        let (server, _) = create_test_server(0x5B, 1).await;
        server
            .send_unicast_offer(recv_addr)
            .await
            .expect("send_unicast_offer failed");

        // Receive and parse the offer
        let mut buf = vec![0u8; 65535];
        let (len, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            receiver.recv_from(&mut buf),
        )
        .await
        .expect("Timeout waiting for OfferService")
        .unwrap();

        let view = MessageView::parse(&buf[..len]).unwrap();
        assert_eq!(view.header().message_id(), crate::protocol::MessageId::SD);
        let sd_view = view.sd_header().unwrap();
        let mut entries = sd_view.entries();
        let entry = entries.next().unwrap();
        assert_eq!(entry.entry_type().unwrap(), sd::EntryType::OfferService);
        assert_eq!(entry.service_id(), 0x5B);
        assert_eq!(entry.instance_id(), 1);

        // Also test that start_announcing doesn't error
        drop(server);
        let (server2, _) = create_test_server(0x5B, 1).await;
        assert!(server2.start_announcing().is_ok());
    }

    #[tokio::test]
    async fn test_run_non_sd_message() {
        let (mut server, server_port) = create_test_server(0x5B, 1).await;
        let client_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_port = match client_socket.local_addr().unwrap() {
            std::net::SocketAddr::V4(a) => a.port(),
            _ => panic!("expected v4"),
        };

        let subscriptions = Arc::clone(&server.subscriptions);

        let server_handle = tokio::spawn(async move {
            server.run().await.ok();
        });

        // Send a non-SD SOME/IP message (service 0x1234, method 0x0001)
        let non_sd_header = SomeIpHeader::new(
            crate::protocol::MessageId::new_from_service_and_method(0x1234, 0x0001),
            0x0001,
            0x01,
            0x01,
            MessageTypeField::new(MessageType::Request, false),
            ReturnCode::Ok,
            0,
        );
        let mut non_sd_buf = Vec::new();
        non_sd_header.encode(&mut non_sd_buf).unwrap();
        client_socket
            .send_to(&non_sd_buf, format!("127.0.0.1:{}", server_port))
            .await
            .unwrap();

        // Small delay, then send valid subscribe
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let message = make_subscription_header(
            0x5B,
            1,
            1,
            3,
            0x01,
            Ipv4Addr::new(127, 0, 0, 1),
            sd::TransportProtocol::Udp,
            client_port,
        );
        client_socket
            .send_to(&message, format!("127.0.0.1:{}", server_port))
            .await
            .unwrap();

        // Wait for ACK
        let mut resp_buf = vec![0u8; 65535];
        let (resp_len, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client_socket.recv_from(&mut resp_buf),
        )
        .await
        .expect("Timeout waiting for SubscribeAck")
        .unwrap();

        let ttl = parse_subscribe_ack_ttl(&resp_buf[..resp_len]);
        assert!(ttl > 0, "Expected ACK (TTL > 0), got TTL={}", ttl);

        // Verify subscription was added (non-SD message was ignored)
        let subs = subscriptions.read().await;
        assert_eq!(subs.subscription_count(), 1);

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_run_malformed_data() {
        let (mut server, server_port) = create_test_server(0x5B, 1).await;
        let client_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_port = match client_socket.local_addr().unwrap() {
            std::net::SocketAddr::V4(a) => a.port(),
            _ => panic!("expected v4"),
        };

        let subscriptions = Arc::clone(&server.subscriptions);

        let server_handle = tokio::spawn(async move {
            server.run().await.ok();
        });

        // Send garbage bytes
        client_socket
            .send_to(&[0xFF, 0xFE, 0xFD], format!("127.0.0.1:{}", server_port))
            .await
            .unwrap();

        // Small delay, then send valid subscribe
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let message = make_subscription_header(
            0x5B,
            1,
            1,
            3,
            0x01,
            Ipv4Addr::new(127, 0, 0, 1),
            sd::TransportProtocol::Udp,
            client_port,
        );
        client_socket
            .send_to(&message, format!("127.0.0.1:{}", server_port))
            .await
            .unwrap();

        // Wait for ACK
        let mut resp_buf = vec![0u8; 65535];
        let (resp_len, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client_socket.recv_from(&mut resp_buf),
        )
        .await
        .expect("Timeout waiting for SubscribeAck")
        .unwrap();

        let ttl = parse_subscribe_ack_ttl(&resp_buf[..resp_len]);
        assert!(ttl > 0, "Expected ACK (TTL > 0), got TTL={}", ttl);

        let subs = subscriptions.read().await;
        assert_eq!(subs.subscription_count(), 1);

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_next_sd_session_id_wraps() {
        let (server, _) = create_test_server(0x5B, 1).await;

        // Set session ID to 0xFFFE
        server.sd_session_id.store(0xFFFE, Ordering::Relaxed);

        // First call: 0xFFFE -> 0xFFFF, returns 0xFFFF
        let sid1 = server.next_sd_session_id();
        assert_eq!(sid1, 0xFFFF);

        // Second call: 0xFFFF -> wraps to 0x0001 (skipping 0), returns 0x0001
        let sid2 = server.next_sd_session_id();
        assert_eq!(sid2, 0x0001);
    }

    #[tokio::test]
    async fn test_handle_sd_other_entry_type() {
        let (mut server, _) = create_test_server(0x5B, 1).await;

        // Build SD message with a StopOfferService entry (not handled by server)
        let entry = sd::Entry::StopOfferService(sd::ServiceEntry {
            index_first_options_run: 0,
            index_second_options_run: 0,
            options_count: sd::OptionsCount::new(0, 0),
            service_id: 0x5B,
            instance_id: 1,
            major_version: 1,
            ttl: 0,
            minor_version: 0,
        });
        let stop_entries = [entry];
        let sd_msg = sd::Header::new(Flags::new(true, true), &stop_entries, &[]);

        // Encode and parse through view types
        let mut buf = [0u8; 64];
        let n = sd_msg.encode(&mut buf.as_mut_slice()).unwrap();
        let sd_view = sd::SdHeaderView::parse(&buf[..n]).unwrap();

        // Should not panic or error
        let result = server
            .handle_sd_message(&sd_view, "127.0.0.1:12345".parse().unwrap())
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_subscribe_ack_different_endpoint_port() {
        let (mut server, server_port) = create_test_server(0x5B, 1).await;
        let client_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let message = make_subscription_header(
            0x5B,
            1,
            1,
            3,
            0x01,
            Ipv4Addr::new(127, 0, 0, 1),
            sd::TransportProtocol::Udp,
            server_port.wrapping_add(1), // Subscriber's port, different from server
        );
        client_socket
            .send_to(&message, format!("127.0.0.1:{}", server_port))
            .await
            .unwrap();

        let server_handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let (len, addr) = server.unicast_socket.recv_from(&mut buf).await.unwrap();
            let data = &buf[..len];
            let view = MessageView::parse(data).unwrap();
            let sd_view = view.sd_header().unwrap();
            server.handle_sd_message(&sd_view, addr).await.unwrap();

            // Subscription should have been added
            let subs = server.subscriptions.read().await;
            assert_eq!(subs.subscription_count(), 1);
        });

        let mut resp_buf = vec![0u8; 65535];
        let (resp_len, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client_socket.recv_from(&mut resp_buf),
        )
        .await
        .expect("Timeout waiting for SubscribeAck")
        .unwrap();

        let ttl = parse_subscribe_ack_ttl(&resp_buf[..resp_len]);
        assert!(ttl > 0, "Expected ACK (TTL > 0), got TTL={}", ttl);

        server_handle.await.unwrap();
    }

    // ── extract_subscriber_endpoint ──────────────────────────────────────
    //
    // These tests cover the helper that walks an entry's first/second
    // options runs and returns the first IPv4 endpoint. They use
    // `sd::Options::IpV4Endpoint::write` to build wire bytes directly
    // so we can precisely control what the options array looks like and
    // what indices the entry references.

    /// Serialize one `IpV4Endpoint` option into the given buffer slot.
    /// Returns the number of bytes written (always 12 for `IpV4Endpoint`).
    fn write_ipv4_endpoint_option(
        buf: &mut [u8],
        ip: Ipv4Addr,
        port: u16,
        protocol: sd::TransportProtocol,
    ) -> usize {
        let opt = sd::Options::IpV4Endpoint { ip, protocol, port };
        let mut slot = buf;
        opt.write(&mut slot).unwrap()
    }

    fn write_load_balancing_option(buf: &mut [u8], priority: u16, weight: u16) -> usize {
        let opt = sd::Options::LoadBalancing { priority, weight };
        let mut slot = buf;
        opt.write(&mut slot).unwrap()
    }

    /// Build a byte buffer holding `count` `IpV4Endpoint` options with
    /// successive port numbers starting at `base_port`, and return the
    /// total byte length.
    fn fill_ipv4_endpoints(buf: &mut [u8], count: usize, base_port: u16) -> usize {
        let mut offset = 0;
        for i in 0..count {
            let port_offset = u16::try_from(i).expect("test fixture count fits in u16");
            let n = write_ipv4_endpoint_option(
                &mut buf[offset..],
                Ipv4Addr::new(10, 0, 0, 1),
                base_port + port_offset,
                sd::TransportProtocol::Udp,
            );
            offset += n;
        }
        offset
    }

    #[test]
    fn extract_endpoint_single_option_first_run() {
        let mut buf = [0u8; 64];
        let total = fill_ipv4_endpoints(&mut buf, 1, 30000);
        let iter = sd::OptionIter::new(&buf[..total]);

        let got = Server::extract_subscriber_endpoint(&iter, 0, 1, 0, 0);
        assert_eq!(
            got,
            Some(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 30000))
        );
    }

    #[test]
    fn extract_endpoint_zero_options_in_both_runs_returns_none() {
        let iter = sd::OptionIter::new(&[]);
        assert_eq!(Server::extract_subscriber_endpoint(&iter, 0, 0, 0, 0), None);
    }

    #[test]
    fn extract_endpoint_count_zero_with_nonzero_index_returns_none() {
        // An entry with first_count = 0 at a non-zero index must not
        // dereference anything, even if the options array has data past
        // that index.
        let mut buf = [0u8; 64];
        let total = fill_ipv4_endpoints(&mut buf, 2, 30100);
        let iter = sd::OptionIter::new(&buf[..total]);

        assert_eq!(Server::extract_subscriber_endpoint(&iter, 1, 0, 0, 0), None);
    }

    #[test]
    fn extract_endpoint_multi_option_first_run_returns_first() {
        // Two IpV4Endpoint options in the first run. The helper should
        // return the first and log a warning about the second. We just
        // verify the return value here.
        let mut buf = [0u8; 64];
        let total = fill_ipv4_endpoints(&mut buf, 2, 30200);
        let iter = sd::OptionIter::new(&buf[..total]);

        let got = Server::extract_subscriber_endpoint(&iter, 0, 2, 0, 0);
        assert_eq!(
            got,
            Some(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 30200))
        );
    }

    #[test]
    fn extract_endpoint_split_across_first_and_second_runs() {
        // Three options [A, B, C]. Entry references option A in the
        // first run (first_index=0, first_count=1) and option C in the
        // second run (second_index=2, second_count=1). We expect to
        // pick A — the first run is walked first — and we also expect
        // a multi-endpoint warning because the helper collects endpoints
        // from BOTH runs without deduplication and sees two total.
        let mut buf = [0u8; 96];
        let total = fill_ipv4_endpoints(&mut buf, 3, 30300);
        let iter = sd::OptionIter::new(&buf[..total]);

        let got = Server::extract_subscriber_endpoint(&iter, 0, 1, 2, 1);
        assert_eq!(
            got,
            Some(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 30300))
        );
    }

    #[test]
    fn extract_endpoint_honors_first_index_offset() {
        // Four options [A, B, C, D]. Entry references options starting
        // at index 2 with count 1 — that's option C (port 30402).
        let mut buf = [0u8; 128];
        let total = fill_ipv4_endpoints(&mut buf, 4, 30400);
        let iter = sd::OptionIter::new(&buf[..total]);

        let got = Server::extract_subscriber_endpoint(&iter, 2, 1, 0, 0);
        assert_eq!(
            got,
            Some(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 30402))
        );
    }

    #[test]
    fn extract_endpoint_respects_first_count_cap() {
        // If first_count=1 but there are more options after the starting
        // index, we must NOT accidentally pick up the later ones.
        let mut buf = [0u8; 128];
        let total = fill_ipv4_endpoints(&mut buf, 4, 30500);
        let iter = sd::OptionIter::new(&buf[..total]);

        // Take only 1 option starting at index 1 -> port 30501.
        let got = Server::extract_subscriber_endpoint(&iter, 1, 1, 0, 0);
        assert_eq!(
            got,
            Some(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 30501))
        );
    }

    #[test]
    fn extract_endpoint_skips_non_ipv4_options() {
        // Build options = [LoadBalancing, IpV4Endpoint, LoadBalancing].
        // Entry references all three in the first run. We must return
        // the single IpV4Endpoint (at index 1) and skip the other two.
        let mut buf = [0u8; 64];
        let mut offset = 0;
        offset += write_load_balancing_option(&mut buf[offset..], 1, 2);
        offset += write_ipv4_endpoint_option(
            &mut buf[offset..],
            Ipv4Addr::new(10, 0, 0, 1),
            30600,
            sd::TransportProtocol::Udp,
        );
        offset += write_load_balancing_option(&mut buf[offset..], 3, 4);
        let iter = sd::OptionIter::new(&buf[..offset]);

        let got = Server::extract_subscriber_endpoint(&iter, 0, 3, 0, 0);
        assert_eq!(
            got,
            Some(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 30600))
        );
    }

    #[test]
    fn extract_endpoint_all_non_ipv4_returns_none() {
        let mut buf = [0u8; 32];
        let mut offset = 0;
        offset += write_load_balancing_option(&mut buf[offset..], 1, 2);
        offset += write_load_balancing_option(&mut buf[offset..], 3, 4);
        let iter = sd::OptionIter::new(&buf[..offset]);

        assert_eq!(Server::extract_subscriber_endpoint(&iter, 0, 2, 0, 0), None);
    }

    #[test]
    fn extract_endpoint_second_run_only() {
        // Two options, entry references only the second one via the
        // second_options_run pair.
        let mut buf = [0u8; 64];
        let total = fill_ipv4_endpoints(&mut buf, 2, 30700);
        let iter = sd::OptionIter::new(&buf[..total]);

        let got = Server::extract_subscriber_endpoint(&iter, 0, 0, 1, 1);
        assert_eq!(
            got,
            Some(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 30701))
        );
    }

    /// End-to-end regression: drive a real `Server::handle_sd_message` with
    /// a single SD packet that carries *two* entries — an `OfferService`
    /// referencing option index 0 and a `SubscribeEventGroup` referencing
    /// option index 1 — where each option is a different
    /// `IpV4Endpoint`. The subscription recorded by the server must use the
    /// Subscribe entry's endpoint (options[1]), not the first option in
    /// the packet (options[0]).
    ///
    /// Before the `extract_subscriber_endpoint` fix, the server would
    /// silently take options[0] for every subscribe and register the
    /// wrong endpoint.
    #[tokio::test]
    async fn combined_sd_subscribe_uses_its_own_options_run() {
        let (mut server, _port) = create_test_server(0x5B, 1).await;

        let offer_endpoint_port: u16 = 40_111;
        let subscribe_endpoint_port: u16 = 40_222;

        // Entry 0: OfferService for (0x5B, instance 1) — references
        // options[0] (the offer's own endpoint).
        let offer_entry = Entry::OfferService(sd::ServiceEntry {
            index_first_options_run: 0,
            index_second_options_run: 0,
            options_count: sd::OptionsCount::new(1, 0),
            service_id: 0x5B,
            instance_id: 1,
            major_version: 1,
            ttl: 3,
            minor_version: 0,
        });
        // Entry 1: SubscribeEventGroup for (0x5B, instance 1, eg 0x01) —
        // references options[1] (the subscriber's endpoint).
        let subscribe_entry = Entry::SubscribeEventGroup(sd::EventGroupEntry {
            index_first_options_run: 1,
            index_second_options_run: 0,
            options_count: sd::OptionsCount::new(1, 0),
            service_id: 0x5B,
            instance_id: 1,
            major_version: 1,
            ttl: 3,
            counter: 0,
            event_group_id: 0x0001,
        });
        let entries = [offer_entry, subscribe_entry];
        let options = [
            sd::Options::IpV4Endpoint {
                ip: Ipv4Addr::LOCALHOST,
                protocol: sd::TransportProtocol::Udp,
                port: offer_endpoint_port,
            },
            sd::Options::IpV4Endpoint {
                ip: Ipv4Addr::LOCALHOST,
                protocol: sd::TransportProtocol::Udp,
                port: subscribe_endpoint_port,
            },
        ];
        let sd_header = sd::Header::new(
            sd::Flags::new_sd(sd::RebootFlag::Continuous),
            &entries,
            &options,
        );
        let message = build_sd_message(&sd_header);

        // Send the combined SD message to the server's SD socket from a
        // fresh client socket and have the server handle exactly one
        // datagram. We drive `handle_sd_message` directly rather than
        // `server.run()` so we can assert state after the call.
        let client_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let sd_addr = match server.sd_socket.local_addr().unwrap() {
            std::net::SocketAddr::V4(v4) => v4,
            std::net::SocketAddr::V6(_) => panic!("expected v4 sd socket"),
        };
        client_socket.send_to(&message, sd_addr).await.unwrap();

        let mut buf = vec![0u8; 65_535];
        let (len, sender) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            server.sd_socket.recv_from(&mut buf),
        )
        .await
        .expect("timeout receiving combined SD packet")
        .unwrap();
        let view = MessageView::parse(&buf[..len]).unwrap();
        let sd_view = view.sd_header().unwrap();
        server.handle_sd_message(&sd_view, sender).await.unwrap();

        // The server must have registered exactly one subscriber, and
        // its endpoint must be the SubscribeEventGroup entry's options[1]
        // endpoint — NOT the OfferService entry's options[0] endpoint.
        let subs = server.subscriptions.read().await;
        let subscribers = subs.get_subscribers(0x5B, 1, 0x0001);
        assert_eq!(
            subscribers.len(),
            1,
            "combined SD packet must yield exactly one subscriber"
        );
        assert_eq!(
            subscribers[0].address.port(),
            subscribe_endpoint_port,
            "subscription endpoint must come from the Subscribe entry's own \
             options run (options[1]={subscribe_endpoint_port}), not from \
             the Offer entry's options[0]={offer_endpoint_port}"
        );
        assert_ne!(
            subscribers[0].address.port(),
            offer_endpoint_port,
            "regression: subscription picked up the OfferService endpoint \
             instead of its own SubscribeEventGroup endpoint"
        );
    }

    // ── Server::new_passive and passive misuse guards ───────────────────
    //
    // These tests cover the passive-server path added for clients that
    // drive SD through a shared Client discovery socket rather than the
    // Server's own SD socket.

    /// Construct a passive server on loopback with an ephemeral unicast
    /// port. Tests use this as a standard fixture.
    async fn make_passive_server(service_id: u16, instance_id: u16) -> Server {
        let config = ServerConfig::new(Ipv4Addr::LOCALHOST, 0, service_id, instance_id);
        Server::new_passive(config)
            .await
            .expect("new_passive should succeed")
    }

    #[tokio::test]
    async fn new_passive_unicast_bound_to_requested_port() {
        let server = make_passive_server(0x005C, 0x0001).await;
        let local = server.unicast_local_addr().unwrap();
        match local {
            std::net::SocketAddr::V4(v4) => {
                assert_ne!(
                    v4.port(),
                    0,
                    "kernel should assign an ephemeral port when local_port=0"
                );
            }
            std::net::SocketAddr::V6(_) => panic!("expected IPv4 unicast address"),
        }
    }

    #[tokio::test]
    async fn new_passive_sd_socket_is_not_bound_to_30490() {
        // The whole point of a passive server is that its SD socket is
        // NOT in the SO_REUSEPORT group at port 30490. We check directly
        // against the internal `sd_socket` field since tests live in
        // the same module.
        let server = make_passive_server(0x005C, 0x0001).await;
        let sd_addr = server.sd_socket.local_addr().unwrap();
        match sd_addr {
            std::net::SocketAddr::V4(v4) => {
                assert_ne!(
                    v4.port(),
                    30490,
                    "passive SD socket must not bind the SOME/IP SD port"
                );
            }
            std::net::SocketAddr::V6(_) => panic!("expected IPv4 SD address"),
        }
    }

    #[tokio::test]
    async fn new_passive_publisher_accepts_register_subscriber() {
        // End-to-end: construct a passive server, get its publisher,
        // register a subscriber via the external path, and verify the
        // publisher sees it.
        let server = make_passive_server(0x005C, 0x0001).await;
        let publisher = server.publisher();

        assert!(!publisher.has_subscribers(0x005C, 0x0001, 0x0001).await);

        let subscriber = SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 2), 40_000);
        publisher
            .register_subscriber(0x005C, 0x0001, 0x0001, subscriber)
            .await;

        assert!(publisher.has_subscribers(0x005C, 0x0001, 0x0001).await);
        assert_eq!(publisher.subscriber_count(0x005C, 0x0001, 0x0001).await, 1);

        // Clean up via the symmetric API.
        publisher
            .remove_subscriber(0x005C, 0x0001, 0x0001, subscriber)
            .await;
        assert!(!publisher.has_subscribers(0x005C, 0x0001, 0x0001).await);
    }

    #[tokio::test]
    async fn start_announcing_on_passive_returns_invalid_input() {
        let server = make_passive_server(0x005C, 0x0001).await;
        let err = server
            .start_announcing()
            .expect_err("start_announcing on a passive server must fail");
        match err {
            Error::Io(io_err) => {
                assert_eq!(io_err.kind(), std::io::ErrorKind::InvalidInput);
                let msg = format!("{io_err}");
                assert!(
                    msg.contains("passive"),
                    "error message should mention 'passive': {msg}"
                );
                assert!(
                    msg.contains("0x005C"),
                    "error message should include the service_id: {msg}"
                );
            }
            other => panic!("expected Error::Io(InvalidInput), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_on_passive_returns_invalid_input() {
        let mut server = make_passive_server(0x005C, 0x0001).await;
        let err = server
            .run()
            .await
            .expect_err("run on a passive server must fail");
        match err {
            Error::Io(io_err) => {
                assert_eq!(io_err.kind(), std::io::ErrorKind::InvalidInput);
                let msg = format!("{io_err}");
                assert!(
                    msg.contains("passive"),
                    "error message should mention 'passive': {msg}"
                );
                assert!(
                    msg.contains("0x005C"),
                    "error message should include the service_id: {msg}"
                );
            }
            other => panic!("expected Error::Io(InvalidInput), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn start_announcing_on_regular_server_still_succeeds() {
        // Regression guard: the new is_passive check must not break the
        // standard non-passive path.
        let (server, _port) = create_test_server(0x005C, 0x0001).await;
        server
            .start_announcing()
            .expect("start_announcing on a regular server must still succeed");
        // The announcer task runs forever; the test succeeds as soon as
        // start_announcing returns Ok. The spawned task is cleaned up
        // when the Tokio test runtime shuts down at the end of this
        // test — `tokio::spawn` tasks are not aborted by dropping
        // unrelated handles, they ride the runtime lifecycle.
    }

    #[tokio::test]
    async fn new_passive_two_instances_do_not_fight_over_sd_port() {
        // Two passive servers on the same interface must both construct
        // successfully — they would collide if either tried to bind
        // 30490, but since they each bind an ephemeral SD placeholder
        // port, they stay out of each other's way.
        let a = make_passive_server(0x005B, 0x0002).await;
        let b = make_passive_server(0x005C, 0x0001).await;

        let addr_a = a.sd_socket.local_addr().unwrap();
        let addr_b = b.sd_socket.local_addr().unwrap();
        // Different placeholder ports.
        assert_ne!(addr_a, addr_b);
        // And neither is 30490.
        if let std::net::SocketAddr::V4(v4) = addr_a {
            assert_ne!(v4.port(), 30490);
        }
        if let std::net::SocketAddr::V4(v4) = addr_b {
            assert_ne!(v4.port(), 30490);
        }
    }

    #[tokio::test]
    async fn new_passive_returns_error_when_unicast_bind_fails() {
        // Bind a unicast port first so the subsequent `new_passive` call
        // collides on (interface, local_port) — covers the `?` error
        // path on the unicast `UdpSocket::bind` inside `new_passive`.
        let blocker = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("blocker bind should succeed");
        let blocker_port = match blocker.local_addr().unwrap() {
            std::net::SocketAddr::V4(v4) => v4.port(),
            std::net::SocketAddr::V6(_) => panic!("expected IPv4"),
        };

        let config = ServerConfig::new(Ipv4Addr::LOCALHOST, blocker_port, 0x005C, 0x0001);
        let result = Server::new_passive(config).await;
        let Err(err) = result else {
            panic!("new_passive must fail when the unicast port is taken");
        };
        match err {
            Error::Io(io_err) => {
                assert!(
                    matches!(
                        io_err.kind(),
                        std::io::ErrorKind::AddrInUse | std::io::ErrorKind::PermissionDenied
                    ),
                    "expected AddrInUse or PermissionDenied, got {:?}",
                    io_err.kind()
                );
            }
            other => panic!("expected Error::Io, got {other:?}"),
        }
        drop(blocker);
    }

    #[tokio::test]
    async fn new_passive_with_tracing_subscriber_evaluates_format_args() {
        // Coverage helper: with no global tracing subscriber, `tracing::info!`
        // and `tracing::debug!` short-circuit before evaluating their
        // formatted arguments, leaving the format-arg lines in `new_passive`
        // marked as uncovered. This test installs a max-level subscriber so
        // the macros take their full format path and the arg-evaluation
        // regions show as covered.
        use tracing::subscriber::with_default;
        use tracing_subscriber::fmt;

        let subscriber = fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_test_writer()
            .finish();

        let fut = async {
            let _server = make_passive_server(0x00AA, 0x00BB).await;
        };
        // `with_default` only applies to the synchronous block where it is
        // installed, so we drive the future to completion inside the block
        // by repeatedly polling it on a manual executor — but the simplest
        // approach is to use `block_on` of an inner runtime. Since we are
        // already inside a tokio test, we instead spawn the work onto a
        // thread that owns its own runtime.
        let handle = std::thread::spawn(move || {
            with_default(subscriber, || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(fut);
            });
        });
        handle.join().expect("subscriber thread panicked");
    }

    #[test]
    fn extract_subscriber_endpoint_with_tracing_evaluates_log_args() {
        // Coverage helper: with no global tracing subscriber, the format
        // args of the `warn!` (no endpoint, multi-endpoint) and `trace!`
        // (single endpoint) macros inside `extract_subscriber_endpoint`
        // are not evaluated and show as uncovered. This test installs a
        // TRACE-level subscriber and exercises all three branches so the
        // arg-evaluation regions become covered.
        use tracing::subscriber::with_default;
        use tracing_subscriber::fmt;

        let subscriber = fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_test_writer()
            .finish();

        with_default(subscriber, || {
            // 0 endpoints → warn! "No IPv4 endpoint" branch.
            let iter_empty = sd::OptionIter::new(&[]);
            assert_eq!(
                Server::extract_subscriber_endpoint(&iter_empty, 0, 0, 0, 0),
                None
            );

            // 1 endpoint → trace! "Found IPv4 endpoint" branch.
            let mut buf_one = [0u8; 32];
            let len_one = fill_ipv4_endpoints(&mut buf_one, 1, 31000);
            let iter_one = sd::OptionIter::new(&buf_one[..len_one]);
            assert_eq!(
                Server::extract_subscriber_endpoint(&iter_one, 0, 1, 0, 0),
                Some(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 31000))
            );

            // n endpoints → warn! "{} IPv4 endpoints found" branch.
            let mut buf_many = [0u8; 64];
            let len_many = fill_ipv4_endpoints(&mut buf_many, 3, 31100);
            let iter_many = sd::OptionIter::new(&buf_many[..len_many]);
            assert_eq!(
                Server::extract_subscriber_endpoint(&iter_many, 0, 3, 0, 0),
                Some(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 31100))
            );
        });
    }
}
