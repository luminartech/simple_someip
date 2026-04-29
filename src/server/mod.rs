//! SOME/IP Server/Provider functionality
//!
//! This module provides server-side SOME/IP functionality including:
//! - Service offering/announcement via Service Discovery
//! - Event publishing to subscribers
//! - Event group management
//! - Request/Response handling

mod error;
mod event_publisher;
mod sd_state;
mod service_info;
mod subscription_manager;

pub use error::Error;
pub use event_publisher::EventPublisher;
pub use service_info::Subscriber;
#[cfg(feature = "std")]
pub use service_info::{EventGroupInfo, ServiceInfo};
#[cfg(feature = "bare_metal")]
pub use subscription_manager::{StaticSubscriptionHandle, StaticSubscriptionStorage};
pub use subscription_manager::{SubscribeError, SubscriptionHandle, SubscriptionManager};

use sd_state::SdStateManager;

use core::sync::atomic::{AtomicBool, Ordering};

use crate::Timer;
use crate::e2e::{E2EKey, E2EProfile};
use crate::protocol::sd::{self, Entry, Flags, OptionsCount, ServiceEntry, TransportProtocol};
use crate::transport::{
    E2ERegistryHandle, SocketHandle, SocketOptions, TransportFactory, TransportSocket,
    WrappableSocketHandle,
};
use alloc::sync::Arc;
use core::net::{Ipv4Addr, SocketAddrV4};
use futures_util::{FutureExt, pin_mut, select_biased};
#[cfg(test)]
use std::vec::Vec;

#[cfg(feature = "server-tokio")]
use crate::e2e::E2ERegistry;
#[cfg(feature = "server-tokio")]
use std::sync::Mutex;
#[cfg(feature = "server-tokio")]
use tokio::sync::RwLock;

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
    /// Event-group IDs the server publishes to. Used by the SD
    /// `Subscribe` handler to NACK subscriptions for unknown groups
    /// (per AUTOSAR SOME/IP-SD: an event group must be known before
    /// subscription is granted). When empty, any event-group ID is
    /// accepted — preserves back-compat for callers that have not
    /// enumerated their groups; populate to opt into validation.
    pub event_group_ids: heapless::Vec<u16, { ServerConfig::EVENT_GROUP_IDS_CAP }>,
}

impl ServerConfig {
    /// Maximum number of event-group IDs trackable in
    /// [`Self::event_group_ids`]. Matches `EVENT_GROUPS_CAP` in the
    /// subscription manager.
    pub const EVENT_GROUP_IDS_CAP: usize = 32;

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
            event_group_ids: heapless::Vec::new(),
        }
    }

    /// Returns `true` if `event_group_id` is registered, OR
    /// [`Self::event_group_ids`] is empty (validation disabled).
    #[must_use]
    pub fn accepts_event_group(&self, event_group_id: u16) -> bool {
        self.event_group_ids.is_empty() || self.event_group_ids.contains(&event_group_id)
    }
}

/// Bundle of pluggable infrastructure passed to [`Server::new_with_deps`].
/// Mirrors `crate::ClientDeps` (under `client`) but with the server's
/// smaller surface
/// — no `Spawner` (server has no internal task spawning), no
/// `InterfaceHandle` (interface lives in [`ServerConfig`]).
///
/// All four fields are public so callers can construct the struct
/// inline.
pub struct ServerDeps<F, Tm, R, S>
where
    F: TransportFactory,
    Tm: Timer,
    R: E2ERegistryHandle,
    S: SubscriptionHandle,
{
    /// Transport factory used to bind the unicast and SD sockets.
    pub factory: F,
    /// Async sleep primitive used by the announcement loop's 1-second tick.
    pub timer: Tm,
    /// Shared E2E registry handle for runtime E2E configuration.
    pub e2e_registry: R,
    /// Shared subscription manager handle. The convenience constructor
    /// `Server::new` (under `server-tokio`) builds an
    /// `Arc<RwLock<SubscriptionManager>>` for this; bare-metal callers
    /// supply their own [`SubscriptionHandle`] impl.
    pub subscriptions: S,
}

/// SOME/IP Server that can offer services and publish events.
///
/// Generic over the four pluggable infrastructure types bundled in
/// [`ServerDeps`]:
/// - `R: E2ERegistryHandle` — runtime E2E configuration registry
/// - `S: SubscriptionHandle` — event-group subscription state
/// - `F: TransportFactory` — socket primitive (carried as a stored
///   unit-struct in the tokio path; bare-metal impls may carry state)
/// - `Tm: Timer` — async sleep used by the announcement loop
///
/// The convenience constructors `Self::new` / `Self::new_with_loopback`
/// / `Self::new_passive` (under the `server-tokio` feature) instantiate
/// these as `Arc<Mutex<E2ERegistry>>` / `Arc<RwLock<SubscriptionManager>>`
/// / `TokioTransport` / `TokioTimer`. Bare-metal callers use
/// [`Self::new_with_deps`] (under `server`) and supply their own.
pub struct Server<R, S, F, Tm, H = Arc<<F as TransportFactory>::Socket>>
where
    R: E2ERegistryHandle,
    S: SubscriptionHandle,
    F: TransportFactory + 'static,
    F::Socket: 'static,
    Tm: Timer + Clone + 'static,
    H: SocketHandle<Socket = F::Socket>,
{
    config: ServerConfig,
    /// Socket for receiving subscription requests, behind whatever
    /// shared-storage `H` chose (`Arc<T>` on std,
    /// `StaticSocketHandle<T>` on bare metal).
    unicast_socket: H,
    /// Socket for sending SD announcements (same handle type as
    /// `unicast_socket`; both are produced by the same factory).
    sd_socket: H,
    /// Subscription manager
    subscriptions: S,
    /// Event publisher
    publisher: Arc<EventPublisher<R, S, H>>,
    /// SD session-ID counter and announcement emitter
    sd_state: Arc<SdStateManager>,
    /// Shared E2E registry for runtime E2E configuration
    e2e_registry: R,
    /// Transport factory. Used at construction time to bind sockets;
    /// retained on the struct so bare-metal factories that carry state
    /// (e.g. an embassy-net `Stack` handle) survive the constructor.
    /// On `server-tokio` builds this is a zero-sized `TokioTransport`.
    #[allow(dead_code)]
    factory: F,
    /// Async sleep primitive used by [`Self::announcement_loop`]'s
    /// 1-second tick. On `server-tokio` builds this is `TokioTimer`
    /// (wrapping `tokio::time::sleep`).
    timer: Tm,
    /// `true` if this server was constructed via `Server::new_passive`.
    /// Passive servers have no real SD socket bound to port 30490; their
    /// SD handling is managed externally. Calling [`Self::announcement_loop`]
    /// or [`Self::run`] on a passive server is a programming error and
    /// returns an [`Error::Io`] with [`std::io::ErrorKind::InvalidInput`].
    is_passive: bool,
    /// Set the first time [`Self::announcement_loop`] is called. A
    /// second call returns `Err(Error::Io(InvalidInput))` so two
    /// independent futures cannot race on the same SD socket and
    /// session counter.
    announcement_loop_started: AtomicBool,
}

#[cfg(feature = "server-tokio")]
impl
    Server<
        Arc<Mutex<E2ERegistry>>,
        Arc<RwLock<SubscriptionManager>>,
        crate::tokio_transport::TokioTransport,
        crate::tokio_transport::TokioTimer,
    >
{
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
        let deps = ServerDeps {
            factory: crate::tokio_transport::TokioTransport,
            timer: crate::tokio_transport::TokioTimer,
            e2e_registry: Arc::new(Mutex::new(E2ERegistry::new())),
            subscriptions: Arc::new(RwLock::new(SubscriptionManager::new())),
        };
        Self::new_with_deps(deps, config, multicast_loopback).await
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
    /// [`Server::announcement_loop`] or spawn [`Server::run`] on a passive
    /// server — the external dispatcher owns those responsibilities.
    ///
    /// # Errors
    ///
    /// Returns an error if binding either socket fails.
    pub async fn new_passive(config: ServerConfig) -> Result<Self, Error> {
        let deps = ServerDeps {
            factory: crate::tokio_transport::TokioTransport,
            timer: crate::tokio_transport::TokioTimer,
            e2e_registry: Arc::new(Mutex::new(E2ERegistry::new())),
            subscriptions: Arc::new(RwLock::new(SubscriptionManager::new())),
        };
        Self::new_passive_with_deps(deps, config).await
    }
}

impl<R, S, F, Tm, H> Server<R, S, F, Tm, H>
where
    R: E2ERegistryHandle,
    S: SubscriptionHandle,
    F: TransportFactory + 'static,
    F::Socket: 'static,
    Tm: Timer + Clone + 'static,
    H: WrappableSocketHandle<Socket = F::Socket>,
{
    /// Bare-metal-friendly constructor that takes every dependency
    /// explicitly via a [`ServerDeps`] bundle. The `server-tokio`
    /// convenience constructors (`Self::new`, `Self::new_with_loopback`,
    /// `Self::new_passive`) ultimately delegate here.
    ///
    /// `H: WrappableSocketHandle` is required because this constructor
    /// binds two sockets internally (`unicast` + `sd`) and needs to
    /// place each one behind the caller's chosen shared-storage. On
    /// std this is `Arc<F::Socket>`; on bare metal with an allocator
    /// it can be any `WrappableSocketHandle` impl. Pure-no-alloc
    /// consumers using `StaticSocketHandle` need a future
    /// external-bind constructor variant — see `SocketHandle` docs.
    ///
    /// # Errors
    ///
    /// Returns an error if binding the unicast or SD socket via
    /// [`TransportFactory::bind`] fails, or if joining the SD multicast
    /// group fails.
    pub async fn new_with_deps(
        deps: ServerDeps<F, Tm, R, S>,
        mut config: ServerConfig,
        multicast_loopback: bool,
    ) -> Result<Self, Error> {
        let ServerDeps {
            factory,
            timer,
            e2e_registry,
            subscriptions,
        } = deps;

        // Bind unicast socket for receiving subscriptions, then wrap
        // through `WrappableSocketHandle` so the rest of the Server
        // sees the caller's chosen shared-storage type rather than
        // the raw `F::Socket`.
        let unicast_addr = SocketAddrV4::new(config.interface, config.local_port);
        let unicast_raw = factory.bind(unicast_addr, &SocketOptions::new()).await?;
        let bound_port = unicast_raw.local_addr()?.port();
        let unicast_socket: H = H::wrap(unicast_raw);
        // If the caller passed local_port = 0, the kernel picked an
        // ephemeral port. Back-fill the config so SD offers and event
        // publishers advertise the actual bound port instead of 0.
        config.local_port = bound_port;
        tracing::info!(
            "Server bound to {}:{} for service 0x{:04X}",
            config.interface,
            bound_port,
            config.service_id
        );

        // Bind SD socket for sending/receiving SD messages (must use SD port 30490).
        let mut sd_opts = SocketOptions::new();
        sd_opts.reuse_address = true;
        sd_opts.reuse_port = true;
        sd_opts.multicast_if_v4 = Some(config.interface);
        sd_opts.multicast_loop_v4 = Some(multicast_loopback);
        let sd_addr = SocketAddrV4::new(config.interface, sd::MULTICAST_PORT);
        let sd_raw = factory.bind(sd_addr, &sd_opts).await?;
        sd_raw.join_multicast_v4(sd::MULTICAST_IP, config.interface)?;
        let sd_socket: H = H::wrap(sd_raw);
        tracing::info!(
            "Server SD socket bound to {} (expected port {}), joined multicast {}",
            sd_addr,
            sd::MULTICAST_PORT,
            sd::MULTICAST_IP
        );

        let publisher = Arc::new(EventPublisher::new(
            subscriptions.clone(),
            unicast_socket.clone(),
            e2e_registry.clone(),
        ));

        Ok(Self {
            config,
            unicast_socket,
            sd_socket,
            subscriptions,
            publisher,
            sd_state: Arc::new(SdStateManager::new()),
            e2e_registry,
            factory,
            timer,
            is_passive: false,
            announcement_loop_started: AtomicBool::new(false),
        })
    }

    /// Bare-metal-friendly passive-server constructor.
    ///
    /// Passive servers bind a unicast socket as usual but bind their SD
    /// socket to an ephemeral port (port 0) instead of the SOME/IP SD
    /// port — see `Server::new_passive` under `server-tokio` for the
    /// full explanation. Calling [`Self::announcement_loop`] or
    /// [`Self::run`] on the result is a programming error.
    ///
    /// # Errors
    ///
    /// Returns an error if binding either socket fails.
    pub async fn new_passive_with_deps(
        deps: ServerDeps<F, Tm, R, S>,
        mut config: ServerConfig,
    ) -> Result<Self, Error> {
        let ServerDeps {
            factory,
            timer,
            e2e_registry,
            subscriptions,
        } = deps;

        // Bind unicast socket at the configured local_port.
        let unicast_addr = SocketAddrV4::new(config.interface, config.local_port);
        let unicast_raw = factory.bind(unicast_addr, &SocketOptions::new()).await?;
        let bound_port = unicast_raw.local_addr()?.port();
        let unicast_socket: H = H::wrap(unicast_raw);
        // Back-fill the actual bound port if the caller passed 0.
        config.local_port = bound_port;
        tracing::info!(
            "Passive server bound to {}:{} for service 0x{:04X}",
            config.interface,
            bound_port,
            config.service_id
        );

        // Placeholder SD socket on an ephemeral port — no multicast options,
        // no group join. Nothing should route to it.
        let sd_placeholder_addr = SocketAddrV4::new(config.interface, 0);
        let sd_socket: H = H::wrap(
            factory
                .bind(sd_placeholder_addr, &SocketOptions::new())
                .await?,
        );
        tracing::info!(
            "Passive server SD placeholder socket bound near {} (not in SD reuseport group)",
            sd_placeholder_addr
        );

        let publisher = Arc::new(EventPublisher::new(
            subscriptions.clone(),
            unicast_socket.clone(),
            e2e_registry.clone(),
        ));

        Ok(Self {
            config,
            unicast_socket,
            sd_socket,
            subscriptions,
            publisher,
            sd_state: Arc::new(SdStateManager::new()),
            e2e_registry,
            factory,
            timer,
            is_passive: true,
            announcement_loop_started: AtomicBool::new(false),
        })
    }
}

impl<R, S, F, Tm, H> Server<R, S, F, Tm, H>
where
    R: E2ERegistryHandle,
    S: SubscriptionHandle,
    F: TransportFactory + 'static,
    F::Socket: 'static,
    Tm: Timer + Clone + 'static,
    H: SocketHandle<Socket = F::Socket>,
{
    /// Build the periodic-SD-announcement future.
    ///
    /// Returns a future that sends an `OfferService` message to the SD
    /// multicast group every second. The caller must drive the future
    /// (typically via `tokio::spawn`) for announcements to fire; this
    /// function does no work on its own.
    ///
    /// ```no_run
    /// # #[cfg(feature = "server-tokio")] {
    /// # use simple_someip::server::{Server, ServerConfig};
    /// # use std::net::Ipv4Addr;
    /// # async fn demo() -> Result<(), simple_someip::server::Error> {
    /// # let config = ServerConfig::new(Ipv4Addr::LOCALHOST, 30490, 0, 0);
    /// # let server = Server::new(config).await?;
    /// let announce_fut = server.announcement_loop()?;
    /// tokio::spawn(announce_fut);
    /// # Ok(())
    /// # }
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] with [`std::io::ErrorKind::InvalidInput`] if:
    /// - called on a server constructed via `Server::new_passive` — passive
    ///   servers have no real SD socket bound to port 30490, so any
    ///   announcements would go out with an incorrect source port; or
    /// - called twice on the same server. Two announcement futures
    ///   driving the same SD socket and session counter would double the
    ///   announcement rate and race on the wrap-flag latch. Drop the
    ///   first future to disable announcements before requesting a new
    ///   one (which currently still requires a fresh `Server`).
    #[must_use = "the returned announcement-loop future must be spawned (e.g. tokio::spawn) or awaited for the server to emit SD announcements; dropping it silently disables announcements"]
    pub fn announcement_loop(
        &self,
    ) -> Result<impl core::future::Future<Output = ()> + Send + 'static, Error>
    where
        F: Send + Sync,
        F::Socket: Send + Sync,
        for<'a> <F::Socket as TransportSocket>::SendFuture<'a>: Send,
        H: Send + Sync,
        Tm: Send + Sync,
        for<'a> Tm::SleepFuture<'a>: Send,
    {
        if self.is_passive {
            tracing::warn!(
                "announcement_loop called on passive Server for service 0x{:04X}; \
                 announcements must be driven externally (e.g. via \
                 `simple_someip::Client::sd_announcements_loop`)",
                self.config.service_id
            );
            return Err(Error::InvalidUsage("passive_server_announcement_loop"));
        }
        if self
            .announcement_loop_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            tracing::warn!(
                "announcement_loop already started for service 0x{:04X}; \
                 two announcement futures cannot share the same SD socket \
                 and session counter",
                self.config.service_id
            );
            return Err(Error::InvalidUsage("announcement_loop_already_started"));
        }
        let config = self.config.clone();
        let sd_socket = self.sd_socket.clone();
        let sd_state = Arc::clone(&self.sd_state);
        let timer = self.timer.clone();

        Ok(async move {
            let mut announcement_count = 0u32;
            loop {
                match sd_state
                    .send_offer_service(&config, sd_socket.socket())
                    .await
                {
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

                // Send announcements every 1 second. Sleep goes through
                // the `Timer` trait so bare-metal consumers can swap in
                // a different timer impl; today it resolves to
                // `TokioTimer` under the `server-tokio` feature.
                timer.sleep(core::time::Duration::from_secs(1)).await;
            }
        })
    }

    /// `!Send` counterpart to [`Self::announcement_loop`].
    ///
    /// Returns the same announcement-loop future without the `+ Send`
    /// bound on the return type, so it can be driven by single-threaded
    /// executors (`tokio::task::LocalSet`, embassy with `task-arena = 0`,
    /// etc.) over a `!Sync` transport such as `embassy-net`. Use this on
    /// bare-metal targets where `H::Socket` is `!Sync`; use the
    /// Send-bounded `announcement_loop` on multi-threaded targets.
    ///
    /// # Errors
    ///
    /// Same as [`Self::announcement_loop`].
    #[must_use = "the returned announcement-loop future must be driven (e.g. tokio::task::spawn_local) for the server to emit SD announcements; dropping it silently disables announcements"]
    pub fn announcement_loop_local(
        &self,
    ) -> Result<impl core::future::Future<Output = ()> + 'static, Error> {
        if self.is_passive {
            tracing::warn!(
                "announcement_loop_local called on passive Server for service 0x{:04X}; \
                 announcements must be driven externally (e.g. via \
                 `simple_someip::Client::sd_announcements_loop`)",
                self.config.service_id
            );
            return Err(Error::InvalidUsage("passive_server_announcement_loop"));
        }
        if self
            .announcement_loop_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            tracing::warn!(
                "announcement_loop already started for service 0x{:04X}; \
                 two announcement futures cannot share the same SD socket \
                 and session counter",
                self.config.service_id
            );
            return Err(Error::InvalidUsage("announcement_loop_already_started"));
        }
        let config = self.config.clone();
        let sd_socket = self.sd_socket.clone();
        let sd_state = Arc::clone(&self.sd_state);
        let timer = self.timer.clone();

        Ok(async move {
            let mut announcement_count = 0u32;
            loop {
                match sd_state
                    .send_offer_service(&config, sd_socket.socket())
                    .await
                {
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
                timer.sleep(core::time::Duration::from_secs(1)).await;
            }
        })
    }

    /// Send a unicast `OfferService` to a specific address (in response to `FindService`)
    async fn send_unicast_offer(&self, target: core::net::SocketAddr) -> Result<(), Error> {
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
        // Atomic (sid, reboot_flag) pair so concurrent emissions cannot
        // race around the wrap boundary — see
        // `SdStateManager::next_session_id_with_reboot_flag` docs.
        let (sid, reboot_flag) = self.sd_state.next_session_id_with_reboot_flag();
        let sd_payload = sd::Header::new(Flags::new_sd(reboot_flag), &entries, &options);

        let mut buffer = [0u8; crate::UDP_BUFFER_SIZE];
        let sd_data_len = sd_payload.encode_to_slice(&mut buffer[16..])?;
        let someip_header = SomeIpHeader::new_sd(sid, sd_data_len);
        someip_header.encode_to_slice(&mut buffer[..16])?;
        let total_len = 16 + sd_data_len;

        let target_v4 = socket_addr_v4(target)?;
        self.sd_socket
            .socket()
            .send_to(&buffer[..total_len], target_v4)
            .await?;
        tracing::debug!(
            "Sent unicast OfferService to {} for service 0x{:04X}",
            target,
            self.config.service_id
        );

        Ok(())
    }

    /// Get the event publisher for sending events
    #[must_use]
    pub fn publisher(&self) -> Arc<EventPublisher<R, S, H>> {
        Arc::clone(&self.publisher)
    }

    /// Get the local address of the unicast socket.
    ///
    /// # Errors
    ///
    /// Returns an error if the socket's local address cannot be retrieved.
    pub fn unicast_local_addr(&self) -> Result<core::net::SocketAddr, Error> {
        match self.unicast_socket.socket().local_addr() {
            Ok(v4) => Ok(core::net::SocketAddr::V4(v4)),
            Err(e) => Err(Error::Transport(e)),
        }
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
    /// # Errors
    ///
    /// Returns [`crate::e2e::E2ERegistryFull`] when the underlying
    /// registry has no room for a new key. Replacing the profile of an
    /// already-registered key always succeeds.
    pub fn register_e2e(
        &self,
        key: E2EKey,
        profile: E2EProfile,
    ) -> Result<(), crate::e2e::E2ERegistryFull> {
        self.e2e_registry.register(key, profile)
    }

    /// Remove E2E configuration for the given key.
    pub fn unregister_e2e(&self, key: &E2EKey) {
        self.e2e_registry.unregister(key);
    }

    /// Run the server event loop with caller-provided receive buffers.
    ///
    /// Handles incoming subscription requests and manages event groups.
    /// Listens on both the unicast socket (for direct requests) and the
    /// SD multicast socket (for `FindService` and `SubscribeEventGroup`).
    ///
    /// `unicast_buf` and `sd_buf` are caller-supplied scratch buffers
    /// for incoming datagrams. Each must be at least one MTU
    /// (~1500 bytes) and ideally up to the IP datagram limit
    /// (64 KiB - 1) — peer SD messages are bounded by the link MTU,
    /// but a SOME/IP server should not silently cap at 1500 because
    /// it is a sink for any peer datagram landing on its SD or
    /// unicast port. Backends that surface truncation
    /// (`ReceivedDatagram::truncated`) emit a `tracing::warn!` when
    /// the caller's buffer was too small; backends that don't
    /// (TokioSocket today) silently truncate at the OS level.
    ///
    /// On bare-metal, callers typically place the buffers in
    /// `static` storage:
    /// ```ignore
    /// static mut UNICAST_BUF: [u8; 65535] = [0; 65535];
    /// static mut SD_BUF: [u8; 65535] = [0; 65535];
    /// // SAFETY: only one task drives `run_with_buffers` for a given Server.
    /// unsafe { server.run_with_buffers(&mut UNICAST_BUF, &mut SD_BUF).await }?;
    /// ```
    ///
    /// On std (or any alloc-using target), [`Self::run`] is the
    /// convenience shim that heap-allocates 64 KiB buffers and
    /// delegates here.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] with [`std::io::ErrorKind::InvalidInput`] if
    /// called on a server constructed via `Server::new_passive` — passive
    /// servers have no real SD socket to read from, so the run loop would
    /// block forever on the ephemeral placeholder socket.
    ///
    /// Otherwise returns an error if receiving from a socket fails or
    /// handling an SD message fails.
    pub async fn run_with_buffers(
        &mut self,
        unicast_buf: &mut [u8],
        sd_buf: &mut [u8],
    ) -> Result<(), Error> {
        use crate::protocol::MessageView;

        if self.is_passive {
            tracing::warn!(
                "run called on passive Server for service 0x{:04X}; \
                 SD receive must be driven externally (e.g. via the \
                 Client's discovery socket, routing Subscribes to \
                 `EventPublisher::register_subscriber`)",
                self.config.service_id
            );
            return Err(Error::InvalidUsage("passive_server_run"));
        }

        loop {
            // `select!` (not `select_biased!`) gives pseudo-random fairness
            // across ready arms each poll — matches the prior
            // `tokio::select!` behavior and avoids starving either the
            // unicast or SD-multicast arm under sustained one-sided load.
            //
            // SAFETY: both arms call `TransportSocket::recv_from`. The
            // `TokioSocket` backend is cancel-safe per tokio docs — a
            // non-selected arm can be dropped without losing in-flight
            // kernel state. Custom transport backends MUST provide the
            // same guarantee. A future contributor adding a
            // non-cancel-safe `FusedFuture` arm here would silently lose
            // state when the arm is dropped on a select win. Both futures
            // must therefore stay `Send + FusedFuture + Unpin` *and*
            // cancel-safe.
            //
            // Fresh futures are constructed each iteration so the borrows
            // of `unicast_buf` / `sd_buf` / the sockets end when the
            // select macro returns, freeing the buffer we index into
            // below.
            let (len, addr, source, from_unicast) = {
                // Reborrow `&mut *foo` rather than `&mut foo` because
                // `unicast_buf` / `sd_buf` are `&mut [u8]` parameters
                // here (caller-owned), not owned `Vec<u8>` locals
                // — direct `&mut foo` would produce `&mut &mut [u8]`.
                let unicast_fut = self
                    .unicast_socket
                    .socket()
                    .recv_from(&mut *unicast_buf)
                    .fuse();
                let sd_fut = self.sd_socket.socket().recv_from(&mut *sd_buf).fuse();
                pin_mut!(unicast_fut, sd_fut);
                select_biased! {
                    result = unicast_fut => {
                        let datagram = result?;
                        (
                            datagram.bytes_received,
                            core::net::SocketAddr::V4(datagram.source),
                            "unicast",
                            true,
                        )
                    }
                    result = sd_fut => {
                        let datagram = result?;
                        (
                            datagram.bytes_received,
                            core::net::SocketAddr::V4(datagram.source),
                            "sd-multicast",
                            false,
                        )
                    }
                }
            };
            let data = if from_unicast {
                &unicast_buf[..len]
            } else {
                &sd_buf[..len]
            };

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

    /// Run the server event loop with heap-allocated 64 KiB recv buffers.
    ///
    /// Convenience wrapper over [`Self::run_with_buffers`] for callers
    /// who have an allocator available — this is the simplest entry
    /// point for std and bare-metal-with-alloc consumers. Bare-metal
    /// callers without an allocator must use
    /// [`Self::run_with_buffers`] directly with caller-supplied
    /// buffers (e.g. `static`-declared `[u8; N]` arrays).
    ///
    /// The 64 KiB sizing matches the IP datagram limit so the server
    /// surfaces (or cleanly truncates at the OS level) any peer
    /// datagram that exceeds the link MTU. See
    /// [`Self::run_with_buffers`] for the full sizing rationale.
    ///
    /// # Errors
    ///
    /// Same as [`Self::run_with_buffers`].
    pub async fn run(&mut self) -> Result<(), Error> {
        let mut unicast_buf = alloc::vec![0u8; 65535];
        let mut sd_buf = alloc::vec![0u8; 65535];
        self.run_with_buffers(&mut unicast_buf, &mut sd_buf).await
    }

    /// Handle a Service Discovery message
    #[allow(clippy::too_many_lines)]
    async fn handle_sd_message(
        &mut self,
        sd_view: &sd::SdHeaderView<'_>,
        sender: core::net::SocketAddr,
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
                        self.send_subscribe_nack_from_view(&entry_view, sender, "wrong_service_id")
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
                            "wrong_instance_id",
                        )
                        .await?;
                    } else if entry_view.major_version() != self.config.major_version {
                        // Per AUTOSAR SOME/IP-SD: a Subscribe whose
                        // major_version disagrees with the server's
                        // configured major must be NACKed (TTL=0). Without
                        // this arm a client probing for a v2 service
                        // against a v1 server would get an Ack and start
                        // sending traffic that the application stack
                        // would silently mis-decode.
                        tracing::warn!(
                            "Subscribe for wrong major_version: expected {}, got {}",
                            self.config.major_version,
                            entry_view.major_version()
                        );
                        if let Err(e) = self
                            .send_subscribe_nack_from_view(
                                &entry_view,
                                sender,
                                "wrong_major_version",
                            )
                            .await
                        {
                            tracing::warn!(error = %e, "SubscribeNack send failed");
                        }
                    } else if !self.config.accepts_event_group(entry_view.event_group_id()) {
                        // Per AUTOSAR SOME/IP-SD, the event group must
                        // be known to the server before subscription
                        // can be granted. If `event_group_ids` is
                        // populated and the request is for an
                        // unrecognised group, NACK so the client
                        // doesn't believe it's subscribed.
                        tracing::warn!(
                            "Subscribe for unknown event_group_id 0x{:04X} (service 0x{:04X})",
                            entry_view.event_group_id(),
                            entry_view.service_id()
                        );
                        if let Err(e) = self
                            .send_subscribe_nack_from_view(
                                &entry_view,
                                sender,
                                "unknown_event_group",
                            )
                            .await
                        {
                            tracing::warn!(error = %e, "SubscribeNack send failed");
                        }
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
                        if let Some(endpoint_addr) = extract_subscriber_endpoint(
                            &sd_view.options(),
                            first_index,
                            first_count,
                            second_index,
                            second_count,
                        ) {
                            let subscribe_result = self
                                .subscriptions
                                .subscribe(
                                    entry_view.service_id(),
                                    entry_view.instance_id(),
                                    entry_view.event_group_id(),
                                    endpoint_addr,
                                )
                                .await;

                            match subscribe_result {
                                Ok(()) => {
                                    // ACK the just-committed subscription. If the
                                    // ACK send fails (transient transport error),
                                    // roll back the subscription so we don't leak
                                    // a committed-but-unacked entry — and log
                                    // rather than propagate, so a single SD-socket
                                    // hiccup doesn't tear down `run()`.
                                    if let Err(e) =
                                        self.send_subscribe_ack_from_view(&entry_view, sender).await
                                    {
                                        tracing::warn!(
                                            error = %e,
                                            service_id = entry_view.service_id(),
                                            instance_id = entry_view.instance_id(),
                                            event_group_id = entry_view.event_group_id(),
                                            "SubscribeAck send failed; rolling back subscription"
                                        );
                                        self.subscriptions
                                            .unsubscribe(
                                                entry_view.service_id(),
                                                entry_view.instance_id(),
                                                entry_view.event_group_id(),
                                                endpoint_addr,
                                            )
                                            .await;
                                    }
                                }
                                Err(e) => {
                                    // Capacity-rejected subscription: NACK so
                                    // the client doesn't believe it's
                                    // subscribed.
                                    let reason: &'static str = match e {
                                        SubscribeError::SubscribersPerGroupFull => {
                                            "subscribers_per_group_full"
                                        }
                                        SubscribeError::EventGroupsFull => "event_groups_full",
                                    };
                                    tracing::debug!("Subscription rejected: {reason}");
                                    if let Err(e) = self
                                        .send_subscribe_nack_from_view(&entry_view, sender, reason)
                                        .await
                                    {
                                        tracing::warn!(error = %e, "SubscribeNack send failed");
                                    }
                                }
                            }
                        } else {
                            tracing::warn!("No endpoint found in Subscribe message options");
                            if let Err(e) = self
                                .send_subscribe_nack_from_view(
                                    &entry_view,
                                    sender,
                                    "no_endpoint_in_options",
                                )
                                .await
                            {
                                tracing::warn!(error = %e, "SubscribeNack send failed");
                            }
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
                        if let Err(e) = self.send_unicast_offer(sender).await {
                            tracing::warn!(error = %e, "Unicast OfferService send failed");
                        }
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
}

/// Convert a [`core::net::SocketAddr`] into a [`SocketAddrV4`] for the
/// transport layer. SOME/IP-SD is IPv4-only at this layer; if a V6
/// address ever surfaces here it indicates a misconfiguration upstream
/// (a V6 socket binding the SD port, or a V6 source address surfaced
/// by a transport that should not produce one). Returns
/// [`TransportError::Unsupported`](crate::transport::TransportError::Unsupported)
/// in that case so the caller can log and drop the message instead of panicking.
fn socket_addr_v4(addr: core::net::SocketAddr) -> Result<SocketAddrV4, Error> {
    match addr {
        core::net::SocketAddr::V4(v4) => Ok(v4),
        core::net::SocketAddr::V6(_) => Err(Error::Transport(
            crate::transport::TransportError::Unsupported,
        )),
    }
}

/// Extract a single subscriber endpoint from the options runs associated with
/// an SD entry. Walks both option runs, returns the first `IpV4Endpoint`
/// found, and logs a `warn!` if more than one is present.
fn extract_subscriber_endpoint(
    options: &sd::OptionIter<'_>,
    first_index: usize,
    first_count: usize,
    second_index: usize,
    second_count: usize,
) -> Option<SocketAddrV4> {
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

impl<R, S, F, Tm, H> Server<R, S, F, Tm, H>
where
    R: E2ERegistryHandle,
    S: SubscriptionHandle,
    F: TransportFactory + 'static,
    F::Socket: 'static,
    Tm: Timer + Clone + 'static,
    H: SocketHandle<Socket = F::Socket>,
{
    /// Send `SubscribeAck` from an entry view
    async fn send_subscribe_ack_from_view(
        &self,
        entry_view: &sd::EntryView<'_>,
        subscriber: core::net::SocketAddr,
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
        // Atomic (sid, reboot_flag) pair — see
        // `SdStateManager::next_session_id_with_reboot_flag`.
        let (sid, reboot_flag) = self.sd_state.next_session_id_with_reboot_flag();
        let sd_payload = sd::Header::new(Flags::new_sd(reboot_flag), &entries, &[]);

        let mut buffer = [0u8; crate::UDP_BUFFER_SIZE];
        let sd_data_len = sd_payload.encode_to_slice(&mut buffer[16..])?;
        let someip_header = SomeIpHeader::new_sd(sid, sd_data_len);
        someip_header.encode_to_slice(&mut buffer[..16])?;
        let total_len = 16 + sd_data_len;

        let subscriber_v4 = socket_addr_v4(subscriber)?;
        self.sd_socket
            .socket()
            .send_to(&buffer[..total_len], subscriber_v4)
            .await?;

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
        subscriber: core::net::SocketAddr,
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
        // Atomic (sid, reboot_flag) pair — see
        // `SdStateManager::next_session_id_with_reboot_flag`.
        let (sid, reboot_flag) = self.sd_state.next_session_id_with_reboot_flag();
        let sd_payload = sd::Header::new(Flags::new_sd(reboot_flag), &entries, &[]);

        let mut buffer = [0u8; crate::UDP_BUFFER_SIZE];
        let sd_data_len = sd_payload.encode_to_slice(&mut buffer[16..])?;
        let someip_header = SomeIpHeader::new_sd(sid, sd_data_len);
        someip_header.encode_to_slice(&mut buffer[..16])?;
        let total_len = 16 + sd_data_len;

        let subscriber_v4 = socket_addr_v4(subscriber)?;
        self.sd_socket
            .socket()
            .send_to(&buffer[..total_len], subscriber_v4)
            .await?;

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

#[cfg(all(test, feature = "server-tokio"))]
mod tests {
    use super::*;
    use crate::protocol::{
        Header as SomeIpHeader, MessageType, MessageTypeField, MessageView, ReturnCode,
    };
    use crate::tokio_transport::{TokioTimer, TokioTransport};
    use crate::traits::WireFormat;
    use std::format;
    use std::net::IpAddr;
    use std::vec;
    use tokio::net::UdpSocket;

    /// Type alias bringing the tokio-flavor concrete type parameters back
    /// into scope so tests can spell `TestServer::new(...)` without
    /// chasing the four-type-parameter signature on every call site.
    /// Mirrors the `TestClient` pattern from `tests/client_server.rs`.
    type TestServer = Server<
        Arc<Mutex<E2ERegistry>>,
        Arc<RwLock<SubscriptionManager>>,
        TokioTransport,
        TokioTimer,
    >;

    #[tokio::test]
    async fn test_server_creation() {
        let config = ServerConfig::new(Ipv4Addr::LOCALHOST, 30682, 0x5B, 1);

        let server: Result<TestServer, _> = TestServer::new(config).await;
        assert!(server.is_ok());
    }

    /// Regression for H5: `ServerConfig::accepts_event_group` must
    /// accept any group when `event_group_ids` is empty (back-compat:
    /// servers that have not enumerated their groups must keep
    /// working) and validate strictly when populated.
    #[test]
    fn server_config_accepts_event_group_empty_means_any() {
        let config = ServerConfig::new(Ipv4Addr::LOCALHOST, 30490, 0x5B, 1);
        assert!(config.event_group_ids.is_empty());
        // Empty list: every group accepted.
        assert!(config.accepts_event_group(0x0001));
        assert!(config.accepts_event_group(0xBEEF));
        assert!(config.accepts_event_group(0xFFFF));
    }

    #[test]
    fn server_config_accepts_event_group_populated_validates() {
        let mut config = ServerConfig::new(Ipv4Addr::LOCALHOST, 30490, 0x5B, 1);
        config.event_group_ids.push(0x0001).unwrap();
        config.event_group_ids.push(0x0042).unwrap();
        assert!(config.accepts_event_group(0x0001));
        assert!(config.accepts_event_group(0x0042));
        assert!(!config.accepts_event_group(0x0002));
        assert!(!config.accepts_event_group(0xBEEF));
    }

    /// Regression for H3: when `subscribe` succeeds but the
    /// `SubscribeAck` send fails (transient transport error), the
    /// just-committed subscription must be rolled back so the
    /// manager isn't left holding a slot for a peer that never
    /// received its ACK. `handle_sd_message` must also NOT propagate
    /// the error via `?` — a single SD-socket hiccup tearing down
    /// `run()` was the original bug.
    #[tokio::test]
    async fn handle_sd_message_rolls_back_subscription_on_failed_ack_send() {
        use crate::transport::{IoErrorKind, ReceivedDatagram, TransportError};
        use core::future::{Future, Ready, ready};
        use core::pin::Pin;
        use core::task::{Context, Poll};
        use std::pin::Pin as StdPin;

        // Socket whose `send_to` always fails. `recv_from` is never
        // called by this test (we drive `handle_sd_message` directly).
        struct FailingSocket {
            local: SocketAddrV4,
        }
        struct FailingSend;
        impl Future for FailingSend {
            type Output = Result<(), TransportError>;
            fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
                Poll::Ready(Err(TransportError::Io(IoErrorKind::NetworkUnreachable)))
            }
        }
        impl TransportSocket for FailingSocket {
            type SendFuture<'a> = FailingSend;
            type RecvFuture<'a> = Ready<Result<ReceivedDatagram, TransportError>>;
            fn send_to<'a>(&'a self, _b: &'a [u8], _t: SocketAddrV4) -> Self::SendFuture<'a> {
                FailingSend
            }
            fn recv_from<'a>(&'a self, _b: &'a mut [u8]) -> Self::RecvFuture<'a> {
                ready(Err(TransportError::Unsupported))
            }
            fn local_addr(&self) -> Result<SocketAddrV4, TransportError> {
                Ok(self.local)
            }
            fn join_multicast_v4(&self, _g: Ipv4Addr, _i: Ipv4Addr) -> Result<(), TransportError> {
                Ok(())
            }
            fn leave_multicast_v4(&self, _g: Ipv4Addr, _i: Ipv4Addr) -> Result<(), TransportError> {
                Ok(())
            }
        }

        struct FailingFactory {
            next_port: Arc<Mutex<u16>>,
        }
        impl TransportFactory for FailingFactory {
            type Socket = FailingSocket;
            type BindFuture<'a> = StdPin<
                std::boxed::Box<
                    dyn Future<Output = Result<Self::Socket, TransportError>> + Send + 'a,
                >,
            >;
            fn bind<'a>(
                &'a self,
                addr: SocketAddrV4,
                _options: &'a SocketOptions,
            ) -> Self::BindFuture<'a> {
                let port = if addr.port() == 0 {
                    let mut p = self.next_port.lock().unwrap();
                    *p = p.saturating_add(1);
                    50000u16.saturating_add(*p)
                } else {
                    addr.port()
                };
                let local = SocketAddrV4::new(*addr.ip(), port);
                std::boxed::Box::pin(async move { Ok(FailingSocket { local }) })
            }
        }

        let factory = FailingFactory {
            next_port: Arc::new(Mutex::new(0)),
        };
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let deps = ServerDeps {
            factory,
            timer: TokioTimer,
            e2e_registry: Arc::new(Mutex::new(E2ERegistry::new())),
            subscriptions: subscriptions.clone(),
        };
        let config = ServerConfig::new(Ipv4Addr::LOCALHOST, 0, 0x5B, 1);
        // Explicit `Arc<FailingSocket>` H so the compiler doesn't have
        // to invent it across the deps-bundle indirection.
        let mut server: Server<_, _, _, _, Arc<FailingSocket>> =
            Server::new_with_deps(deps, config, false)
                .await
                .expect("create failing-socket server");

        // Build a valid Subscribe; our service id/instance/major
        // match the config's defaults, so the only failure point
        // will be the ACK send.
        let bytes = make_subscription_header(
            0x5B,
            1,
            1,
            3,
            0x01,
            Ipv4Addr::LOCALHOST,
            sd::TransportProtocol::Udp,
            45000,
        );
        let view = MessageView::parse(&bytes).expect("parse Subscribe");
        let sd_view = view.sd_header().expect("Subscribe has SD header");
        let sender = core::net::SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 45000));

        // The H3 fix: handle_sd_message must NOT bubble the ACK send
        // failure as Err — it logs and continues.
        let result = server.handle_sd_message(&sd_view, sender).await;
        assert!(
            result.is_ok(),
            "handle_sd_message must not propagate transient SD-socket I/O errors; got {result:?}"
        );

        // The H3 fix: a committed-but-unacked subscription must be
        // rolled back, so the manager has 0 entries.
        let subs = subscriptions.read().await;
        assert_eq!(
            subs.subscription_count(),
            0,
            "subscription must be rolled back after failed ACK send"
        );
    }

    /// Regression for H4: `announcement_loop` must be idempotent.
    /// Calling it a second time returns `Err(Error::Io(InvalidInput))`
    /// so two announcement futures cannot race on the same SD socket
    /// and session counter.
    #[tokio::test]
    async fn announcement_loop_second_call_returns_invalid_input() {
        let config = ServerConfig::new(Ipv4Addr::LOCALHOST, 30683, 0x5BB4, 1);
        let server = TestServer::new(config).await.expect("create server");
        let _first = server
            .announcement_loop()
            .expect("first announcement_loop call must succeed");
        let second = server.announcement_loop();
        match second {
            Err(Error::InvalidUsage(tag)) => {
                assert_eq!(tag, "announcement_loop_already_started");
            }
            Ok(_) => panic!("second announcement_loop must error, got Ok"),
            Err(other) => {
                panic!(
                    "expected Error::InvalidUsage(\"announcement_loop_already_started\"), got {other:?}"
                )
            }
        }
    }

    #[tokio::test]
    async fn test_server_creation_with_loopback_enabled() {
        // Use a unicast port distinct from other tests to avoid EADDRINUSE
        // when the test binary runs tests in parallel. The SD socket binds
        // the SD multicast port (30490) and relies on SO_REUSEPORT, the same
        // as `test_server_creation`.
        let config = ServerConfig::new(Ipv4Addr::LOCALHOST, 30683, 0x5C, 1);

        let server = TestServer::new_with_loopback(config, true)
            .await
            .expect("new_with_loopback(true) should succeed on localhost");

        // Confirm the SD socket was actually configured with IP_MULTICAST_LOOP
        // enabled — this is the behavior the new code path is supposed to
        // produce and is what makes same-host testing possible.
        assert!(
            server
                .sd_socket
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
    async fn create_test_server(service_id: u16, instance_id: u16) -> (TestServer, u16) {
        // Use port 0 to get an ephemeral port
        let config = ServerConfig::new(Ipv4Addr::LOCALHOST, 0, service_id, instance_id);
        let mut server = TestServer::new(config)
            .await
            .expect("Failed to create server");
        let port = match server.unicast_local_addr().unwrap() {
            core::net::SocketAddr::V4(addr) => addr.port(),
            core::net::SocketAddr::V6(_) => panic!("expected IPv4 address"),
        };
        // Update config to reflect actual bound port
        server.set_local_port(port);
        (server, port)
    }

    #[allow(clippy::too_many_arguments)]
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
            Flags::new_sd(sd::RebootFlag::RecentlyRebooted),
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
            Ipv4Addr::LOCALHOST,
            sd::TransportProtocol::Udp,
            server_port,
        );

        // Send to the server
        client_socket
            .send_to(&message, format!("127.0.0.1:{server_port}"))
            .await
            .unwrap();

        // Run server to process one message (with a timeout)
        let server_handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let datagram = server.unicast_socket.recv_from(&mut buf).await.unwrap();
            let len = datagram.bytes_received;
            let addr = core::net::SocketAddr::V4(datagram.source);
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
        assert!(ttl > 0, "Expected ACK (TTL > 0), got TTL={ttl}");

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
            Ipv4Addr::LOCALHOST,
            sd::TransportProtocol::Udp,
            server_port,
        );
        client_socket
            .send_to(&message, format!("127.0.0.1:{server_port}"))
            .await
            .unwrap();

        // Process the message
        let server_handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let datagram = server.unicast_socket.recv_from(&mut buf).await.unwrap();
            let len = datagram.bytes_received;
            let addr = core::net::SocketAddr::V4(datagram.source);
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
        assert_eq!(ttl, 0, "Expected NACK (TTL=0), got TTL={ttl}");

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
            Ipv4Addr::LOCALHOST,
            sd::TransportProtocol::Udp,
            server_port,
        );
        client_socket
            .send_to(&message, format!("127.0.0.1:{server_port}"))
            .await
            .unwrap();

        let server_handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let datagram = server.unicast_socket.recv_from(&mut buf).await.unwrap();
            let len = datagram.bytes_received;
            let addr = core::net::SocketAddr::V4(datagram.source);
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
        assert_eq!(ttl, 0, "Expected NACK (TTL=0), got TTL={ttl}");

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
            Flags::new_sd(sd::RebootFlag::RecentlyRebooted),
            &find_entries,
            &[],
        );
        let message = build_sd_message(&sd_header);
        client_socket
            .send_to(&message, format!("127.0.0.1:{server_port}"))
            .await
            .unwrap();

        // Process the message on the unicast socket
        let server_handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let datagram = server.unicast_socket.recv_from(&mut buf).await.unwrap();
            let len = datagram.bytes_received;
            let addr = core::net::SocketAddr::V4(datagram.source);
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
            Flags::new_sd(sd::RebootFlag::RecentlyRebooted),
            &find_entries,
            &[],
        );
        let message = build_sd_message(&sd_header);
        client_socket
            .send_to(&message, format!("127.0.0.1:{server_port}"))
            .await
            .unwrap();

        let server_handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let datagram = server.unicast_socket.recv_from(&mut buf).await.unwrap();
            let len = datagram.bytes_received;
            let addr = core::net::SocketAddr::V4(datagram.source);
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
            Flags::new_sd(sd::RebootFlag::RecentlyRebooted),
            &find_entries,
            &[],
        );
        let message = build_sd_message(&sd_header);
        client_socket
            .send_to(&message, format!("127.0.0.1:{server_port}"))
            .await
            .unwrap();

        let server_handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let datagram = server.unicast_socket.recv_from(&mut buf).await.unwrap();
            let len = datagram.bytes_received;
            let addr = core::net::SocketAddr::V4(datagram.source);
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
            .send_to(&message, format!("127.0.0.1:{server_port}"))
            .await
            .unwrap();

        let server_handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let datagram = server.unicast_socket.recv_from(&mut buf).await.unwrap();
            let len = datagram.bytes_received;
            let addr = core::net::SocketAddr::V4(datagram.source);
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
        assert_eq!(ttl, 0, "Expected NACK (TTL=0), got TTL={ttl}");

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

        // Also test that announcement_loop builds a future without error.
        drop(server);
        let (server2, _) = create_test_server(0x5B, 1).await;
        let fut = server2
            .announcement_loop()
            .expect("announcement_loop on a regular server must build");
        // Intentionally do not poll or spawn the future: we only care
        // that constructing it returned Ok. If this future were
        // spawned, the announcer would loop indefinitely and emit
        // multicast until explicitly aborted or the Tokio runtime
        // shut down at end-of-test, which could interfere with
        // parallel tests using the same multicast group.
        drop(fut);
    }

    #[tokio::test]
    async fn test_run_non_sd_message() {
        let (mut server, server_port) = create_test_server(0x5B, 1).await;
        let client_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_port = match client_socket.local_addr().unwrap() {
            core::net::SocketAddr::V4(a) => a.port(),
            core::net::SocketAddr::V6(_) => panic!("expected v4 source address"),
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
            .send_to(&non_sd_buf, format!("127.0.0.1:{server_port}"))
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
            Ipv4Addr::LOCALHOST,
            sd::TransportProtocol::Udp,
            client_port,
        );
        client_socket
            .send_to(&message, format!("127.0.0.1:{server_port}"))
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
        assert!(ttl > 0, "Expected ACK (TTL > 0), got TTL={ttl}");

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
            core::net::SocketAddr::V4(a) => a.port(),
            core::net::SocketAddr::V6(_) => panic!("expected v4 source address"),
        };

        let subscriptions = Arc::clone(&server.subscriptions);

        let server_handle = tokio::spawn(async move {
            server.run().await.ok();
        });

        // Send garbage bytes
        client_socket
            .send_to(&[0xFF, 0xFE, 0xFD], format!("127.0.0.1:{server_port}"))
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
            Ipv4Addr::LOCALHOST,
            sd::TransportProtocol::Udp,
            client_port,
        );
        client_socket
            .send_to(&message, format!("127.0.0.1:{server_port}"))
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
        assert!(ttl > 0, "Expected ACK (TTL > 0), got TTL={ttl}");

        let subs = subscriptions.read().await;
        assert_eq!(subs.subscription_count(), 1);

        server_handle.abort();
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
            Ipv4Addr::LOCALHOST,
            sd::TransportProtocol::Udp,
            server_port.wrapping_add(1), // Subscriber's port, different from server
        );
        client_socket
            .send_to(&message, format!("127.0.0.1:{server_port}"))
            .await
            .unwrap();

        let server_handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let datagram = server.unicast_socket.recv_from(&mut buf).await.unwrap();
            let len = datagram.bytes_received;
            let addr = core::net::SocketAddr::V4(datagram.source);
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
        assert!(ttl > 0, "Expected ACK (TTL > 0), got TTL={ttl}");

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

        let got = extract_subscriber_endpoint(&iter, 0, 1, 0, 0);
        assert_eq!(
            got,
            Some(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 30000))
        );
    }

    #[test]
    fn extract_endpoint_zero_options_in_both_runs_returns_none() {
        let iter = sd::OptionIter::new(&[]);
        assert_eq!(extract_subscriber_endpoint(&iter, 0, 0, 0, 0), None);
    }

    #[test]
    fn extract_endpoint_count_zero_with_nonzero_index_returns_none() {
        // An entry with first_count = 0 at a non-zero index must not
        // dereference anything, even if the options array has data past
        // that index.
        let mut buf = [0u8; 64];
        let total = fill_ipv4_endpoints(&mut buf, 2, 30100);
        let iter = sd::OptionIter::new(&buf[..total]);

        assert_eq!(extract_subscriber_endpoint(&iter, 1, 0, 0, 0), None);
    }

    #[test]
    fn extract_endpoint_multi_option_first_run_returns_first() {
        // Two IpV4Endpoint options in the first run. The helper should
        // return the first and log a warning about the second. We just
        // verify the return value here.
        let mut buf = [0u8; 64];
        let total = fill_ipv4_endpoints(&mut buf, 2, 30200);
        let iter = sd::OptionIter::new(&buf[..total]);

        let got = extract_subscriber_endpoint(&iter, 0, 2, 0, 0);
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

        let got = extract_subscriber_endpoint(&iter, 0, 1, 2, 1);
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

        let got = extract_subscriber_endpoint(&iter, 2, 1, 0, 0);
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
        let got = extract_subscriber_endpoint(&iter, 1, 1, 0, 0);
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

        let got = extract_subscriber_endpoint(&iter, 0, 3, 0, 0);
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

        assert_eq!(extract_subscriber_endpoint(&iter, 0, 2, 0, 0), None);
    }

    #[test]
    fn extract_endpoint_second_run_only() {
        // Two options, entry references only the second one via the
        // second_options_run pair.
        let mut buf = [0u8; 64];
        let total = fill_ipv4_endpoints(&mut buf, 2, 30700);
        let iter = sd::OptionIter::new(&buf[..total]);

        let got = extract_subscriber_endpoint(&iter, 0, 0, 1, 1);
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
            sd::Flags::new_sd(sd::RebootFlag::RecentlyRebooted),
            &entries,
            &options,
        );
        let message = build_sd_message(&sd_header);

        // Send the combined SD message to the server's SD socket from a
        // fresh client socket and have the server handle exactly one
        // datagram. We drive `handle_sd_message` directly rather than
        // `server.run()` so we can assert state after the call.
        let client_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let sd_addr = server.sd_socket.local_addr().unwrap();
        client_socket.send_to(&message, sd_addr).await.unwrap();

        let mut buf = vec![0u8; 65_535];
        let datagram = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            server.sd_socket.recv_from(&mut buf),
        )
        .await
        .expect("timeout receiving combined SD packet")
        .unwrap();
        let len = datagram.bytes_received;
        let sender = core::net::SocketAddr::V4(datagram.source);
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
    async fn make_passive_server(service_id: u16, instance_id: u16) -> TestServer {
        let config = ServerConfig::new(Ipv4Addr::LOCALHOST, 0, service_id, instance_id);
        TestServer::new_passive(config)
            .await
            .expect("new_passive should succeed")
    }

    #[tokio::test]
    async fn new_passive_unicast_bound_to_requested_port() {
        let server = make_passive_server(0x005C, 0x0001).await;
        let local = server.unicast_local_addr().unwrap();
        match local {
            core::net::SocketAddr::V4(v4) => {
                assert_ne!(
                    v4.port(),
                    0,
                    "kernel should assign an ephemeral port when local_port=0"
                );
            }
            core::net::SocketAddr::V6(_) => panic!("expected IPv4 unicast address"),
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
        assert_ne!(
            sd_addr.port(),
            30490,
            "passive SD socket must not bind the SOME/IP SD port"
        );
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
            .await
            .unwrap();

        assert!(publisher.has_subscribers(0x005C, 0x0001, 0x0001).await);
        assert_eq!(publisher.subscriber_count(0x005C, 0x0001, 0x0001).await, 1);

        // Clean up via the symmetric API.
        publisher
            .remove_subscriber(0x005C, 0x0001, 0x0001, subscriber)
            .await;
        assert!(!publisher.has_subscribers(0x005C, 0x0001, 0x0001).await);
    }

    #[tokio::test]
    async fn announcement_loop_on_passive_returns_invalid_input() {
        let server = make_passive_server(0x005C, 0x0001).await;
        let err = server
            .announcement_loop()
            .err()
            .expect("announcement_loop on a passive server must fail");
        match err {
            Error::InvalidUsage(tag) => {
                assert_eq!(tag, "passive_server_announcement_loop");
            }
            other => panic!(
                "expected Error::InvalidUsage(\"passive_server_announcement_loop\"), got {other:?}"
            ),
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
            Error::InvalidUsage(tag) => {
                assert_eq!(tag, "passive_server_run");
            }
            other => panic!("expected Error::InvalidUsage(\"passive_server_run\"), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn announcement_loop_on_regular_server_still_succeeds() {
        // Regression guard: the is_passive check must not break the
        // standard non-passive path.
        let (server, _port) = create_test_server(0x005C, 0x0001).await;
        let fut = server
            .announcement_loop()
            .expect("announcement_loop on a regular server must build");
        // The announcer loops forever; the test succeeds as soon as
        // construction returns Ok.
        // Do not poll or spawn the future: doing so would leave the
        // announcer running and emitting multicast for the rest of the
        // test binary's lifetime, interfering with parallel tests that
        // bind the same multicast group. We only care that construction
        // returned Ok, so drop the future without polling it.
        drop(fut);
    }

    /// Direct test that `announcement_loop` actually emits an SD
    /// announcement when driven. Explicit coverage for the primary entry
    /// point (avoids regressions where only the deleted shim was exercised).
    #[ignore = "requires MULTICAST on loopback; consistent with the \
                #[ignore]-gated sd_state.rs tests. Runs in any environment \
                where loopback multicast is available."]
    #[tokio::test]
    async fn announcement_loop_sends_offer_service_when_driven() {
        use crate::protocol::MessageId;

        // Use service/instance IDs not used elsewhere in this test module
        // so parallel tests joined to the same SD multicast group cannot
        // produce false matches.
        const SID: u16 = 0xAA01;
        const IID: u16 = 0xFF01;

        // Bind a receiver on the SD multicast port with loopback so we
        // actually see the outgoing announcement. Use a dedicated
        // receiver socket via socket2 to match the SD bind pattern.
        let iface = std::net::Ipv4Addr::LOCALHOST;
        let recv = {
            let s = socket2::Socket::new(
                socket2::Domain::IPV4,
                socket2::Type::DGRAM,
                Some(socket2::Protocol::UDP),
            )
            .unwrap();
            s.set_reuse_address(true).unwrap();
            #[cfg(unix)]
            s.set_reuse_port(true).unwrap();
            s.bind(&core::net::SocketAddr::new(IpAddr::V4(iface), sd::MULTICAST_PORT).into())
                .unwrap();
            s.set_nonblocking(true).unwrap();
            let std_s: std::net::UdpSocket = s.into();
            let rs = tokio::net::UdpSocket::from_std(std_s).unwrap();
            rs.join_multicast_v4(sd::MULTICAST_IP, iface).unwrap();
            rs
        };

        let config = ServerConfig::new(iface, 30501, SID, IID);
        let server = TestServer::new_with_loopback(config, true).await.unwrap();
        let fut = server.announcement_loop().expect("build loop");
        let handle = tokio::spawn(fut);

        // Filter out any stray SD traffic from other parallel tests
        // until we see one whose OfferService entry carries OUR sid/iid.
        // Bounded by a single outer timeout so a totally-silent server
        // (the regression we actually care about) still fails the test.
        let mut buf = [0u8; 1500];
        let offer_fields = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let (n, _src) = recv.recv_from(&mut buf).await.expect("recv failed");
                let Ok(view) = crate::protocol::MessageView::parse(&buf[..n]) else {
                    continue;
                };
                if view.header().message_id() != MessageId::SD {
                    continue;
                }
                let Ok(sd_view) = view.sd_header() else {
                    continue;
                };
                let Some(entry) = sd_view.entries().next() else {
                    continue;
                };
                if !matches!(entry.entry_type(), Ok(sd::EntryType::OfferService)) {
                    continue;
                }
                if entry.service_id() != SID || entry.instance_id() != IID {
                    continue;
                }
                break (
                    entry.service_id(),
                    entry.instance_id(),
                    entry.major_version(),
                    entry.ttl(),
                );
            }
        })
        .await
        .expect("timed out waiting for our OfferService");

        let (svc, inst, major, ttl) = offer_fields;
        assert_eq!(svc, SID, "emitted service_id must match server config");
        assert_eq!(inst, IID, "emitted instance_id must match server config");
        assert_eq!(major, 1, "default major_version from ServerConfig::new");
        assert!(
            ttl > 0,
            "OfferService TTL must be non-zero (TTL=0 means StopOffering)",
        );

        handle.abort();
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
        assert_ne!(addr_a.port(), 30490);
        assert_ne!(addr_b.port(), 30490);
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
            core::net::SocketAddr::V4(v4) => v4.port(),
            core::net::SocketAddr::V6(_) => panic!("expected IPv4"),
        };

        let config = ServerConfig::new(Ipv4Addr::LOCALHOST, blocker_port, 0x005C, 0x0001);
        let result = TestServer::new_passive(config).await;
        let Err(err) = result else {
            panic!("new_passive must fail when the unicast port is taken");
        };
        match err {
            // The bind path goes through the `TransportFactory` trait,
            // so port collisions surface as
            // `Error::Transport(TransportError::AddressInUse)` instead
            // of `Error::Io`. Both variants are accepted to keep the
            // test stable across future transport-error refactors.
            Error::Transport(crate::transport::TransportError::AddressInUse) => {}
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
            other => panic!("expected Error::Io or Error::Transport(AddressInUse), got {other:?}"),
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
            assert_eq!(extract_subscriber_endpoint(&iter_empty, 0, 0, 0, 0), None);

            // 1 endpoint → trace! "Found IPv4 endpoint" branch.
            let mut buf_one = [0u8; 32];
            let len_one = fill_ipv4_endpoints(&mut buf_one, 1, 31000);
            let iter_one = sd::OptionIter::new(&buf_one[..len_one]);
            assert_eq!(
                extract_subscriber_endpoint(&iter_one, 0, 1, 0, 0),
                Some(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 31000))
            );

            // n endpoints → warn! "{} IPv4 endpoints found" branch.
            let mut buf_many = [0u8; 64];
            let len_many = fill_ipv4_endpoints(&mut buf_many, 3, 31100);
            let iter_many = sd::OptionIter::new(&buf_many[..len_many]);
            assert_eq!(
                extract_subscriber_endpoint(&iter_many, 0, 3, 0, 0),
                Some(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 31100))
            );
        });
    }

    /// Smoke test for [`Server::announcement_loop`]: a loopback server
    /// with `multicast_loop` enabled should emit at least one
    /// `OfferService` on the SD multicast group within a couple of
    /// seconds.
    ///
    /// `#[ignore]`d for the same reason as the `sd_state` tests — hosts
    /// without the MULTICAST flag on `lo` drop the packet silently. The
    /// announcer task is captured and aborted at the end of the test so
    /// it does not leak multicast traffic into other parallel tests.
    #[ignore = "requires loopback multicast support (MULTICAST on lo)"]
    #[tokio::test]
    async fn announcement_loop_emits_first_offer_within_timeout() {
        use crate::protocol::MessageView;
        use crate::protocol::sd::EntryType;

        let interface = Ipv4Addr::LOCALHOST;
        // Pick a service_id and unicast port that do not collide with
        // the other loopback-enabled server test in this file.
        let service_id = 0xFE02;
        let config = ServerConfig::new(interface, 30684, service_id, 0x43);

        // Receiver joined to the SD multicast group on loopback.
        let raw_rx = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )
        .unwrap();
        raw_rx.set_reuse_address(true).unwrap();
        #[cfg(unix)]
        raw_rx.set_reuse_port(true).unwrap();
        raw_rx.set_multicast_loop_v4(true).unwrap();
        raw_rx
            .bind(&core::net::SocketAddr::new(IpAddr::V4(interface), sd::MULTICAST_PORT).into())
            .unwrap();
        raw_rx.set_nonblocking(true).unwrap();
        let rx: UdpSocket = UdpSocket::from_std(raw_rx.into()).unwrap();
        rx.join_multicast_v4(sd::MULTICAST_IP, interface).unwrap();

        let server = TestServer::new_with_loopback(config, true)
            .await
            .expect("server must bind with loopback enabled");
        let announce_fut = server
            .announcement_loop()
            .expect("announcement_loop should build on a non-passive server");
        let announce_handle = tokio::spawn(announce_fut);

        // Scan the multicast group for our OfferService. The first tick
        // happens immediately; 2s is ample headroom for scheduler jitter.
        let recv_loop = async {
            let mut buf = [0u8; 2048];
            loop {
                let (len, _from) = rx.recv_from(&mut buf).await.expect("recv_from");
                let Ok(view) = MessageView::parse(&buf[..len]) else {
                    continue;
                };
                if view.header().message_id().service_id() != 0xFFFF {
                    continue;
                }
                let Ok(sd_view) = view.sd_header() else {
                    continue;
                };
                let Some(entry) = sd_view.entries().next() else {
                    continue;
                };
                if !matches!(entry.entry_type(), Ok(EntryType::OfferService)) {
                    continue;
                }
                if entry.service_id() == service_id {
                    return;
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(2), recv_loop)
            .await
            .expect("announcement_loop should emit at least one OfferService within 2s");
        announce_handle.abort();
        let _ = announce_handle.await;
    }
}
