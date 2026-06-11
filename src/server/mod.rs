//! SOME/IP Server/Provider functionality
//!
//! This module provides server-side SOME/IP functionality including:
//! - Service offering/announcement via Service Discovery
//! - Event publishing to subscribers
//! - Event group management
//! - Request/Response handling

mod error;
mod event_publisher;
mod runtime;
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

pub use sd_state::SdStateManager;

use core::sync::atomic::{AtomicBool, Ordering};

use crate::Timer;
use crate::e2e::{E2EKey, E2EProfile};
#[cfg(feature = "_alloc")]
use crate::protocol::sd;
#[cfg(test)]
use crate::protocol::sd::{Entry, Flags, ServiceEntry};
#[cfg(feature = "_alloc")]
use crate::transport::SocketOptions;
#[cfg(feature = "_alloc")]
use crate::transport::WrappableSharedHandle;
use crate::transport::{E2ERegistryHandle, SharedHandle, TransportFactory, TransportSocket};
#[cfg(feature = "_alloc")]
use alloc::sync::Arc;
use core::net::Ipv4Addr;
#[cfg(feature = "_alloc")]
use core::net::SocketAddrV4;
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
    /// Whether the run-future drives the SD `OfferService` announcement
    /// loop. Defaults to `true`.
    ///
    /// Set to `false` (via [`Self::with_announce`]) when an external
    /// component drives announcements — for example the
    /// `examples/client_server` topology where a co-located `Client`'s
    /// `sd_announcements_loop` emits the offers and the server should
    /// stay silent on SD. Has no effect on passive servers, which never
    /// announce.
    pub announce: bool,
}

impl ServerConfig {
    /// Maximum number of event-group IDs trackable in
    /// [`Self::event_group_ids`]. Matches `EVENT_GROUPS_CAP` in the
    /// subscription manager.
    pub const EVENT_GROUP_IDS_CAP: usize = 32;

    /// Create a new server configuration with sane defaults for
    /// development.
    ///
    /// Required arguments are the SOME/IP `service_id` and
    /// `instance_id` — the two values that identify the offered
    /// service. Other fields use development-friendly defaults that
    /// production callers will typically override via the fluent
    /// setters:
    ///
    /// | Field | Default | Override via |
    /// |---|---|---|
    /// | `interface` | [`Ipv4Addr::UNSPECIFIED`] (`0.0.0.0`) | [`Self::with_interface`] |
    /// | `local_port` | `0` (kernel-assigned ephemeral) | [`Self::with_local_port`] |
    /// | `major_version` | `1` | [`Self::with_major_version`] |
    /// | `minor_version` | `0` | [`Self::with_minor_version`] |
    /// | `ttl` | 3 seconds (typical for SOME/IP) | [`Self::with_ttl`] |
    /// | `event_group_ids` | empty (any group accepted) | [`Self::with_event_group`] |
    ///
    /// Production deployments almost always need a specific interface
    /// and port — `0.0.0.0` lets the kernel pick a binding that may
    /// not match the service's E/E-architecture wiring expectations,
    /// and an ephemeral port can't be discovered by peers without a
    /// separate side-channel. Treat the defaults as "good enough to
    /// stand up a test server in three lines" rather than
    /// production-ready.
    ///
    /// # Example
    ///
    /// ```
    /// use simple_someip::server::ServerConfig;
    /// use std::net::Ipv4Addr;
    ///
    /// let config = ServerConfig::new(0x5BAA, 1)
    ///     .with_interface(Ipv4Addr::new(192, 168, 1, 100))
    ///     .with_local_port(30500);
    /// ```
    #[must_use]
    pub fn new(service_id: u16, instance_id: u16) -> Self {
        Self {
            interface: Ipv4Addr::UNSPECIFIED,
            local_port: 0,
            service_id,
            instance_id,
            major_version: 1,
            minor_version: 0,
            ttl: 3, // 3 seconds is typical for SOME/IP
            event_group_ids: heapless::Vec::new(),
            announce: true,
        }
    }

    /// Set the local interface IP address. Defaults to
    /// [`Ipv4Addr::UNSPECIFIED`] (`0.0.0.0`) from [`Self::new`] —
    /// production deployments will almost always override this to
    /// match their E/E-architecture wiring.
    #[must_use]
    pub fn with_interface(mut self, interface: Ipv4Addr) -> Self {
        self.interface = interface;
        self
    }

    /// Set the local UDP port the server listens on for subscription
    /// requests and unicast traffic. Defaults to `0` from
    /// [`Self::new`] (kernel-assigned ephemeral port), which is fine
    /// for tests but cannot be discovered by external peers and
    /// should be set explicitly in production.
    #[must_use]
    pub fn with_local_port(mut self, local_port: u16) -> Self {
        self.local_port = local_port;
        self
    }

    /// Returns `true` if `event_group_id` is registered, OR
    /// [`Self::event_group_ids`] is empty (validation disabled).
    #[must_use]
    pub fn accepts_event_group(&self, event_group_id: u16) -> bool {
        self.event_group_ids.is_empty() || self.event_group_ids.contains(&event_group_id)
    }

    // ── Fluent builder ───────────────────────────────────────────────
    //
    // Each `with_*` setter consumes and returns `self` so callers can
    // chain overrides starting from `Self::new(...)`. The struct's
    // public fields stay available; the builder is just a less-noisy
    // path for the common "constructor + a couple of overrides" shape.

    /// Set the SOME/IP major version. Defaults to `1` from
    /// [`Self::new`].
    #[must_use]
    pub fn with_major_version(mut self, major_version: u8) -> Self {
        self.major_version = major_version;
        self
    }

    /// Set the SOME/IP minor version. Defaults to `0` from
    /// [`Self::new`].
    #[must_use]
    pub fn with_minor_version(mut self, minor_version: u32) -> Self {
        self.minor_version = minor_version;
        self
    }

    /// Set the SD announcement TTL. Defaults to 3 seconds from
    /// [`Self::new`] (typical for SOME/IP).
    ///
    /// The SOME/IP-SD wire format encodes TTL as `u32` whole seconds;
    /// sub-second precision in the supplied `Duration` is truncated
    /// (rounded down). Durations exceeding `u32::MAX` seconds (~136
    /// years) saturate to `u32::MAX`. The reserved special value
    /// `0xFFFFFF` ("until next reboot") can be requested by passing
    /// `Duration::from_secs(0xFFFFFF)`.
    #[must_use]
    pub fn with_ttl(mut self, ttl: core::time::Duration) -> Self {
        self.ttl = u32::try_from(ttl.as_secs()).unwrap_or(u32::MAX);
        self
    }

    /// Append an event-group ID to the registered set. Subscriptions
    /// for groups not in this set are NACK'd; an empty set (the
    /// default after [`Self::new`]) accepts any group.
    ///
    /// # Panics
    ///
    /// Panics if more than [`Self::EVENT_GROUP_IDS_CAP`] groups have
    /// been registered. Use [`Self::try_with_event_group`] for the
    /// fallible variant.
    #[must_use]
    pub fn with_event_group(mut self, event_group_id: u16) -> Self {
        self.event_group_ids
            .push(event_group_id)
            .expect("event_group_ids capacity exceeded");
        self
    }

    /// Fallible counterpart to [`Self::with_event_group`].
    ///
    /// # Errors
    ///
    /// Returns the unmodified config (in `Err`) if registering would
    /// exceed [`Self::EVENT_GROUP_IDS_CAP`].
    #[must_use = "the returned `Result` carries the (possibly-modified) config — drop is silent"]
    pub fn try_with_event_group(mut self, event_group_id: u16) -> Result<Self, Self> {
        if self.event_group_ids.push(event_group_id).is_ok() {
            Ok(self)
        } else {
            Err(self)
        }
    }

    /// Set whether the run-future drives the SD `OfferService`
    /// announcement loop. Defaults to `true` from [`Self::new`].
    ///
    /// Pass `false` for the dispatcher topology where a co-located
    /// `Client` drives SD via its own `sd_announcements_loop` and the
    /// server should stay silent on the SD socket. Passive servers
    /// (constructed via `Server::new_passive*`) ignore this setting —
    /// they never announce regardless.
    #[must_use]
    pub fn with_announce(mut self, announce: bool) -> Self {
        self.announce = announce;
        self
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
pub struct ServerDeps<F, Tm, R, Sub>
where
    F: TransportFactory,
    Tm: Timer,
    R: E2ERegistryHandle,
    Sub: SubscriptionHandle,
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
    pub subscriptions: Sub,
    /// Optional `(callback, ctx)` pair invoked from the server's receive
    /// loop for every non-SD **unicast** datagram (method requests /
    /// fire-and-forget calls to offered services). `None` reproduces the
    /// historical "non-SD ignored" behavior. The callback receives the
    /// opaque `ctx` word back verbatim, plus the full raw datagram bytes
    /// and the source `SocketAddrV4`; the consumer is responsible for
    /// re-parsing the SOME/IP header and any E2E check.
    pub non_sd_observer: Option<(NonSdRequestCallback, usize)>,
}

/// Tokio-defaulted constructor.
///
/// Available under the `server-tokio` feature. Returns a `ServerDeps`
/// pre-populated with `TokioTransport` / `TokioTimer` and a fresh
/// `Arc<Mutex<E2ERegistry>>` / `Arc<RwLock<SubscriptionManager>>`.
/// Combine with the [`ServerDeps::with_factory`] /
/// [`ServerDeps::with_timer`] / [`ServerDeps::with_e2e_registry`] /
/// [`ServerDeps::with_subscriptions`] builders to override individual
/// fields without spelling out the rest by hand.
///
/// ```no_run
/// # #[cfg(feature = "server-tokio")]
/// # async fn demo() -> Result<(), simple_someip::server::Error> {
/// use simple_someip::{Server, ServerDeps};
/// use simple_someip::server::ServerConfig;
/// use std::net::Ipv4Addr;
/// let deps = ServerDeps::tokio();
/// let config = ServerConfig::new(0x1234, 1).with_interface(Ipv4Addr::LOCALHOST).with_local_port(0);
/// // The binding-site type fixes Server's `H`/`Hsd`/`Hep` to their
/// // `Arc<…>` defaults so type inference doesn't have to chase them.
/// let (_server, _handles, _run): (Server<_, _, _, _>, _, _) =
///     Server::new_with_deps(deps, config, false).await?;
/// # Ok(())
/// # }
/// ```
#[cfg(feature = "server-tokio")]
impl
    ServerDeps<
        crate::tokio_transport::TokioTransport,
        crate::tokio_transport::TokioTimer,
        Arc<Mutex<E2ERegistry>>,
        Arc<RwLock<SubscriptionManager>>,
    >
{
    /// Build a `ServerDeps` with the tokio defaults.
    #[must_use]
    pub fn tokio() -> Self {
        Self {
            factory: crate::tokio_transport::TokioTransport,
            timer: crate::tokio_transport::TokioTimer,
            e2e_registry: Arc::new(Mutex::new(E2ERegistry::new())),
            subscriptions: Arc::new(RwLock::new(SubscriptionManager::new())),
            non_sd_observer: None,
        }
    }
}

/// Field-by-field fluent builder. Each `with_*` returns a new
/// `ServerDeps` with that single field replaced (and its corresponding
/// generic parameter updated). Lets callers start from
/// [`ServerDeps::tokio`] and override individual fields without
/// spelling out the full struct literal.
impl<F, Tm, R, Sub> ServerDeps<F, Tm, R, Sub>
where
    F: TransportFactory,
    Tm: Timer,
    R: E2ERegistryHandle,
    Sub: SubscriptionHandle,
{
    /// Replace the `factory` field, returning a `ServerDeps` over the
    /// new factory type.
    pub fn with_factory<F2: TransportFactory>(self, factory: F2) -> ServerDeps<F2, Tm, R, Sub> {
        ServerDeps {
            factory,
            timer: self.timer,
            e2e_registry: self.e2e_registry,
            subscriptions: self.subscriptions,
            non_sd_observer: self.non_sd_observer,
        }
    }

    /// Replace the `timer` field, returning a `ServerDeps` over the new
    /// timer type.
    pub fn with_timer<Tm2: Timer>(self, timer: Tm2) -> ServerDeps<F, Tm2, R, Sub> {
        ServerDeps {
            factory: self.factory,
            timer,
            e2e_registry: self.e2e_registry,
            subscriptions: self.subscriptions,
            non_sd_observer: self.non_sd_observer,
        }
    }

    /// Replace the `e2e_registry` field, returning a `ServerDeps` over
    /// the new registry-handle type.
    pub fn with_e2e_registry<R2: E2ERegistryHandle>(
        self,
        e2e_registry: R2,
    ) -> ServerDeps<F, Tm, R2, Sub> {
        ServerDeps {
            factory: self.factory,
            timer: self.timer,
            e2e_registry,
            subscriptions: self.subscriptions,
            non_sd_observer: self.non_sd_observer,
        }
    }

    /// Replace the `subscriptions` field, returning a `ServerDeps` over
    /// the new subscription-handle type.
    pub fn with_subscriptions<Sub2: SubscriptionHandle>(
        self,
        subscriptions: Sub2,
    ) -> ServerDeps<F, Tm, R, Sub2> {
        ServerDeps {
            factory: self.factory,
            timer: self.timer,
            e2e_registry: self.e2e_registry,
            subscriptions,
            non_sd_observer: self.non_sd_observer,
        }
    }

    /// Register a `(callback, ctx)` pair invoked for every non-SD unicast
    /// datagram (method requests / fire-and-forget calls to offered
    /// services). The opaque `ctx` word is passed back verbatim on every
    /// invocation — FFI callers stash a pointer here as `usize`;
    /// pure-Rust callers that need no context pass `0`. Passing `None`
    /// (the default if unset) preserves the historical "ignore non-SD"
    /// behavior.
    #[must_use]
    pub fn with_non_sd_observer(mut self, observer: Option<(NonSdRequestCallback, usize)>) -> Self {
        self.non_sd_observer = observer;
        self
    }
}

/// Post-construction accessor bundle returned from `Server::new` (and
/// the other constructor variants) alongside the [`Server`] handle and
/// the combined run-future.
///
/// Mirrors `crate::ClientUpdates`'s role on the [`Client`](crate::Client)
/// side: a place to hang things the caller will reach for once
/// construction completes (today: just the
/// [`EventPublisher`](crate::server::EventPublisher) handle; future
/// fields are reserved for forward-compat). Existing
/// `Server::publisher()` accessor is unchanged — the field on this
/// struct is the more discoverable path now that `Server::new` returns
/// it up front.
///
/// The single field is public so callers can destructure inline:
/// ```no_run
/// # #[cfg(feature = "server-tokio")]
/// # async fn demo() -> Result<(), simple_someip::server::Error> {
/// use simple_someip::Server;
/// use simple_someip::server::ServerConfig;
/// use std::net::Ipv4Addr;
/// let config = ServerConfig::new(0x1234, 1)
///     .with_interface(Ipv4Addr::LOCALHOST)
///     .with_local_port(0);
/// let (_server, handles, run) = Server::new(config).await?;
/// let _publisher = handles.publisher;
/// tokio::spawn(run);
/// # Ok(())
/// # }
/// ```
pub struct ServerHandles<Hep> {
    /// `EventPublisher` handle for emitting events from the server side
    /// (clone of the field on [`Server`]; included here so the common
    /// destructuring pattern doesn't have to call `.publisher()`
    /// separately).
    pub publisher: Hep,
}

/// Bundle of pre-built dependencies + storage handles for
/// [`Server::new_with_handles`] / [`Server::new_passive_with_handles`].
///
/// Variant of [`ServerDeps`] for callers who have already bound
/// their sockets externally and assembled storage handles
/// themselves — the bare-metal-no-alloc path. Each
/// `Wrappable*Handle`-using constructor on the alloc path
/// (`Server::new_with_deps`, `Server::new_passive_with_deps`) has a
/// counterpart here that takes pre-built handles directly,
/// skipping the internal `wrap` step. That lets a no-alloc consumer
/// supply `&'static EmbassyNetSocket` /
/// `&'static SdStateManager` / `&'static EventPublisher<...>`
/// instances they materialized via their preferred static-storage
/// pattern (the blanket `SharedHandle<T>` impl on `&'static T`
/// makes the `&'static …` shape a drop-in for the `Arc<…>` shape).
///
/// All eight fields are public so the struct can be assembled
/// inline.
pub struct ServerStorage<F, Tm, R, Sub, H, Hsd, Hep>
where
    F: TransportFactory + 'static,
    Tm: Timer,
    R: E2ERegistryHandle,
    Sub: SubscriptionHandle,
    H: SharedHandle<F::Socket>,
    Hsd: SharedHandle<SdStateManager>,
    Hep: SharedHandle<EventPublisher<R, Sub, H, F::Socket>>,
{
    /// Transport factory. Retained on the `Server` for any
    /// post-construction state the backend needs to keep alive
    /// (e.g., embassy-net `Stack` handle); the new-with-handles
    /// constructor does NOT call `factory.bind()`.
    pub factory: F,
    /// Async sleep primitive used by the announcement loop's
    /// 1-second tick.
    pub timer: Tm,
    /// Shared E2E registry handle for runtime E2E configuration.
    pub e2e_registry: R,
    /// Shared subscription manager handle.
    pub subscriptions: Sub,
    /// Pre-built unicast socket handle. Caller has already bound
    /// the underlying socket to the desired interface + port.
    pub unicast_socket: H,
    /// Pre-built SD socket handle. For active servers, caller has
    /// bound to the SD multicast port (30490) and joined the SD
    /// multicast group; for passive servers, this is whatever
    /// placeholder socket the caller chose (will not be driven).
    pub sd_socket: H,
    /// Pre-built SD-state handle (`&'static SdStateManager` for
    /// no-alloc, `Arc<SdStateManager>` for alloc).
    pub sd_state: Hsd,
    /// Pre-built `EventPublisher` handle. For std users this is
    /// typically `Arc<EventPublisher::new(subscriptions, unicast,
    /// e2e)>`; for no-alloc, a `&'static EventPublisher<...>`
    /// declared externally.
    pub publisher: Hep,
    /// First-poll run latch. On alloc builds, pass
    /// `Arc::new(AtomicBool::new(false))`; on no-alloc bare metal, pass
    /// a `&'static AtomicBool` (declared as a `static`). Prevents two
    /// run-futures built from the same `Server` from racing the sockets
    /// and SD session counter.
    pub started: StartedLatch,
    /// Optional `(callback, ctx)` pair for non-SD unicast datagrams
    /// (method requests). `None` reproduces the default "non-SD
    /// ignored" behavior.
    pub non_sd_observer: Option<(NonSdRequestCallback, usize)>,
}

/// SOME/IP Server that can offer services and publish events.
///
/// Generic over the four pluggable infrastructure types bundled in
/// [`ServerDeps`]:
/// - `F: TransportFactory` — socket primitive (carried as a stored
///   unit-struct in the tokio path; bare-metal impls may carry state)
/// - `Tm: Timer` — async sleep used by the announcement loop
/// - `R: E2ERegistryHandle` — runtime E2E configuration registry
/// - `Sub: SubscriptionHandle` — event-group subscription state
///
/// The generic order mirrors [`ServerDeps`] (and, for the shared
/// infrastructure parameters `F`, `Tm`, `R`, the order is also shared
/// with [`crate::ClientDeps`]).
///
/// The convenience constructors `Self::new` / `Self::new_with_loopback`
/// / `Self::new_passive` (under the `server-tokio` feature) instantiate
/// these as `TokioTransport` / `TokioTimer` / `Arc<Mutex<E2ERegistry>>`
/// / `Arc<RwLock<SubscriptionManager>>`. Bare-metal callers use
/// [`Self::new_with_deps`] (under `server`) and supply their own.
/// Default shared-handle types for the `Server`'s `H` / `Hsd` / `Hep`
/// generic parameters. `Arc<T>` when an allocator is present;
/// `&'static T` on no-alloc bare metal (where the caller supplies the
/// statics). Both satisfy `SharedHandle<T>`. These defaults are only
/// materialized for callers that omit the handle parameters (the
/// allocator-backed convenience constructors); no-alloc callers spell
/// the handle types explicitly via `new_with_handles`.
#[cfg(feature = "_alloc")]
type DefaultSocketHandle<F> = Arc<<F as TransportFactory>::Socket>;
#[cfg(not(feature = "_alloc"))]
type DefaultSocketHandle<F> = &'static <F as TransportFactory>::Socket;

#[cfg(feature = "_alloc")]
type DefaultSdStateHandle = Arc<SdStateManager>;
#[cfg(not(feature = "_alloc"))]
type DefaultSdStateHandle = &'static SdStateManager;

#[cfg(feature = "_alloc")]
type DefaultEventPublisherHandle<R, Sub, H, T> = Arc<EventPublisher<R, Sub, H, T>>;
#[cfg(not(feature = "_alloc"))]
type DefaultEventPublisherHandle<R, Sub, H, T> = &'static EventPublisher<R, Sub, H, T>;

pub struct Server<
    F,
    Tm,
    R,
    Sub,
    H = DefaultSocketHandle<F>,
    Hsd = DefaultSdStateHandle,
    Hep = DefaultEventPublisherHandle<R, Sub, H, <F as TransportFactory>::Socket>,
> where
    F: TransportFactory + 'static,
    F::Socket: 'static,
    Tm: Timer + Clone + 'static,
    R: E2ERegistryHandle,
    Sub: SubscriptionHandle,
    H: SharedHandle<F::Socket>,
    Hsd: SharedHandle<SdStateManager>,
    Hep: SharedHandle<EventPublisher<R, Sub, H, F::Socket>>,
{
    config: ServerConfig,
    /// Socket for receiving subscription requests, behind whatever
    /// shared-storage `H` chose (`Arc<T>` on std, `&'static T` on
    /// bare metal — both impls of [`SharedHandle<T>`]).
    unicast_socket: H,
    /// Socket for sending SD announcements (same handle type as
    /// `unicast_socket`; both are produced by the same factory).
    sd_socket: H,
    /// Subscription manager
    subscriptions: Sub,
    /// Event publisher, behind whatever shared-storage `Hep` chose
    /// (`Arc<EventPublisher<R, Sub, H>>` on std,
    /// `&'static EventPublisher<R, Sub, H>` on bare-metal-no-alloc).
    publisher: Hep,
    /// SD session-ID counter and announcement emitter, behind whatever
    /// shared-storage `Hsd` chose (`Arc<SdStateManager>` on std,
    /// `&'static SdStateManager` on bare-metal-no-alloc).
    sd_state: Hsd,
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
    /// SD handling is managed externally. Calling [`Self::run`] on a
    /// passive server is a programming error and returns
    /// [`Error::InvalidUsage`].
    is_passive: bool,
    /// Latch flipped on the first poll of any run-future built from
    /// this `Server`. Subsequent run-futures (whether from the
    /// constructor's tuple, [`Self::run`], or [`Self::run_with_buffers`])
    /// short-circuit with `Err(Error::InvalidUsage("server_already_running"))`
    /// rather than racing on the same SD/unicast sockets and session
    /// counter. Held behind [`StartedLatch`] — `Arc<AtomicBool>` when an
    /// allocator is present, `&'static AtomicBool` on no-alloc bare metal
    /// — because the run-future captures an owned copy independent of
    /// `&self`'s lifetime, and both alternatives are `Clone + 'static`.
    started: StartedLatch,
    /// Optional `(callback, ctx)` pair invoked for non-SD unicast datagrams received
    /// on the service's port (method requests / fire-and-forget calls).
    /// `None` preserves the historical "ignore non-SD" behavior; `Some`
    /// surfaces those datagrams to the consumer (used by halo's FFI to
    /// dispatch HWP1 method requests).
    non_sd_observer: Option<(NonSdRequestCallback, usize)>,
}

/// Callback invoked by the server's `recv_loop` for every non-SD
/// unicast datagram received on the service's port (i.e. method
/// requests / fire-and-forget calls to the offered services). The
/// SOME/IP header is parsed in `recv_loop` and the callback receives
/// decoded fields — the consumer never parses bytes. `payload` is the
/// bytes after the 16-byte SOME/IP header. `e2e_status` is `0`
/// (unchecked) — server-side request E2E is not applied here today.
/// `source` is the sender's address, currently unused by known
/// consumers (future-proofing).
///
/// `ctx` is an opaque caller-owned context word, registered alongside
/// the callback as a `(NonSdRequestCallback, usize)` pair and passed
/// back verbatim on every invocation. It is deliberately `usize`
/// rather than `*mut c_void`: a stored raw pointer would make
/// [`Server`] `!Send` and break [`Server::run`]'s declared `+ Send`
/// bound, while `usize` is trivially `Send + Sync` and matches the
/// `uintptr_t` an FFI caller holds anyway. No `unsafe` enters this
/// crate — the cast back to a pointer (and its safety justification)
/// lives in the consumer's callback body, the only place that knows
/// the pointee's lifetime and thread-safety. Rust-native users that
/// need no context pass `0`. `fn` pointers are
/// `Copy + Send + Sync + 'static`, so the pair can be stored on the
/// `Server` and captured by the run-future without adding a new
/// generic.
pub type NonSdRequestCallback = fn(
    ctx: usize,
    source: core::net::SocketAddrV4,
    service_id: u16,
    method_id: u16,
    payload: &[u8],
    e2e_status: u8,
);

#[cfg(feature = "_alloc")]
type StartedLatch = Arc<AtomicBool>;
#[cfg(not(feature = "_alloc"))]
type StartedLatch = &'static AtomicBool;

/// `Hep` resolved against the `server-tokio` convenience constructors'
/// concrete defaults — the `EventPublisher` shape with all four
/// publisher type parameters bound to their tokio impls. Lets the
/// tokio constructors' `(Self, ServerHandles<…>, run-future)` return
/// type spell out cleanly rather than dragging the four-deep `Arc<…>`
/// chain through every signature.
#[cfg(feature = "server-tokio")]
type DefaultTokioServerHep = Arc<
    EventPublisher<
        Arc<Mutex<E2ERegistry>>,
        Arc<RwLock<SubscriptionManager>>,
        Arc<crate::tokio_transport::TokioSocket>,
        crate::tokio_transport::TokioSocket,
    >,
>;

#[cfg(feature = "server-tokio")]
impl
    Server<
        crate::tokio_transport::TokioTransport,
        crate::tokio_transport::TokioTimer,
        Arc<Mutex<E2ERegistry>>,
        Arc<RwLock<SubscriptionManager>>,
    >
{
    /// Create a new SOME/IP server.
    ///
    /// Returns the `Server` handle for runtime mutation
    /// (`register_e2e`, `publisher`, etc.), a [`ServerHandles`] bundle
    /// destructuring the [`EventPublisher`] up front, and a single
    /// combined run-future the caller spawns to drive both the
    /// receive loop and (unless suppressed via
    /// [`ServerConfig::with_announce`]) the SD announcement loop.
    ///
    /// ```no_run
    /// # #[cfg(feature = "server-tokio")]
    /// # async fn demo() -> Result<(), simple_someip::server::Error> {
    /// use simple_someip::Server;
    /// use simple_someip::server::ServerConfig;
    /// use std::net::Ipv4Addr;
    /// let config = ServerConfig::new(0x1234, 1)
    ///     .with_interface(Ipv4Addr::LOCALHOST)
    ///     .with_local_port(0);
    /// let (_server, handles, run) = Server::new(config).await?;
    /// let _publisher = handles.publisher;
    /// tokio::spawn(run);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if binding the unicast or SD socket fails, or if joining the
    /// SD multicast group fails.
    pub async fn new(
        config: ServerConfig,
    ) -> Result<
        (
            Self,
            ServerHandles<DefaultTokioServerHep>,
            impl core::future::Future<Output = Result<(), Error>> + 'static,
        ),
        Error,
    > {
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
    ) -> Result<
        (
            Self,
            ServerHandles<DefaultTokioServerHep>,
            impl core::future::Future<Output = Result<(), Error>> + 'static,
        ),
        Error,
    > {
        let deps = ServerDeps {
            factory: crate::tokio_transport::TokioTransport,
            timer: crate::tokio_transport::TokioTimer,
            e2e_registry: Arc::new(Mutex::new(E2ERegistry::new())),
            subscriptions: Arc::new(RwLock::new(SubscriptionManager::new())),
            non_sd_observer: None,
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
    pub async fn new_passive(
        config: ServerConfig,
    ) -> Result<
        (
            Self,
            ServerHandles<DefaultTokioServerHep>,
            impl core::future::Future<Output = Result<(), Error>> + 'static,
        ),
        Error,
    > {
        let deps = ServerDeps {
            factory: crate::tokio_transport::TokioTransport,
            timer: crate::tokio_transport::TokioTimer,
            e2e_registry: Arc::new(Mutex::new(E2ERegistry::new())),
            subscriptions: Arc::new(RwLock::new(SubscriptionManager::new())),
            non_sd_observer: None,
        };
        Self::new_passive_with_deps(deps, config).await
    }
}

#[cfg(feature = "_alloc")]
impl<F, Tm, R, Sub, H, Hsd, Hep> Server<F, Tm, R, Sub, H, Hsd, Hep>
where
    F: TransportFactory + 'static,
    F::Socket: 'static,
    Tm: Timer + Clone + 'static,
    R: E2ERegistryHandle,
    Sub: SubscriptionHandle,
    H: WrappableSharedHandle<F::Socket>,
    Hsd: WrappableSharedHandle<SdStateManager>,
    Hep: WrappableSharedHandle<EventPublisher<R, Sub, H, F::Socket>>,
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
    /// it can be any [`WrappableSharedHandle`] impl. Pure-no-alloc
    /// consumers (`&'static T` handles) take pre-built sockets via
    /// [`Self::new_with_handles`] / [`Self::new_passive_with_handles`]
    /// instead.
    ///
    /// # Errors
    ///
    /// Returns an error if binding the unicast or SD socket via
    /// [`TransportFactory::bind`] fails, or if joining the SD multicast
    /// group fails.
    pub async fn new_with_deps(
        deps: ServerDeps<F, Tm, R, Sub>,
        mut config: ServerConfig,
        multicast_loopback: bool,
    ) -> Result<
        (
            Self,
            ServerHandles<Hep>,
            impl core::future::Future<Output = Result<(), Error>> + 'static,
        ),
        Error,
    > {
        let ServerDeps {
            factory,
            timer,
            e2e_registry,
            subscriptions,
            non_sd_observer: deps_non_sd_observer,
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
        crate::log::info!(
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
        crate::log::info!(
            "Server SD socket bound to {} (expected port {}), joined multicast {}",
            sd_addr,
            sd::MULTICAST_PORT,
            sd::MULTICAST_IP
        );

        let publisher = Hep::wrap(EventPublisher::new(
            subscriptions.clone(),
            unicast_socket.clone(),
            e2e_registry.clone(),
        ));

        let server = Self {
            config,
            unicast_socket,
            sd_socket,
            subscriptions,
            publisher,
            sd_state: Hsd::wrap(SdStateManager::new()),
            e2e_registry,
            factory,
            timer,
            is_passive: false,
            started: Arc::new(AtomicBool::new(false)),
            non_sd_observer: deps_non_sd_observer,
        };
        let handles = ServerHandles {
            publisher: server.publisher(),
        };
        let run = server.run_inner();
        Ok((server, handles, run))
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
        deps: ServerDeps<F, Tm, R, Sub>,
        mut config: ServerConfig,
    ) -> Result<
        (
            Self,
            ServerHandles<Hep>,
            impl core::future::Future<Output = Result<(), Error>> + 'static,
        ),
        Error,
    > {
        let ServerDeps {
            factory,
            timer,
            e2e_registry,
            subscriptions,
            non_sd_observer: deps_non_sd_observer,
        } = deps;

        // Bind unicast socket at the configured local_port.
        let unicast_addr = SocketAddrV4::new(config.interface, config.local_port);
        let unicast_raw = factory.bind(unicast_addr, &SocketOptions::new()).await?;
        let bound_port = unicast_raw.local_addr()?.port();
        let unicast_socket: H = H::wrap(unicast_raw);
        // Back-fill the actual bound port if the caller passed 0.
        config.local_port = bound_port;
        crate::log::info!(
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
        crate::log::info!(
            "Passive server SD placeholder socket bound near {} (not in SD reuseport group)",
            sd_placeholder_addr
        );

        let publisher = Hep::wrap(EventPublisher::new(
            subscriptions.clone(),
            unicast_socket.clone(),
            e2e_registry.clone(),
        ));

        let server = Self {
            config,
            unicast_socket,
            sd_socket,
            subscriptions,
            publisher,
            sd_state: Hsd::wrap(SdStateManager::new()),
            e2e_registry,
            factory,
            timer,
            is_passive: true,
            started: Arc::new(AtomicBool::new(false)),
            non_sd_observer: deps_non_sd_observer,
        };
        let handles = ServerHandles {
            publisher: server.publisher(),
        };
        let run = server.run_inner();
        Ok((server, handles, run))
    }
}

impl<F, Tm, R, Sub, H, Hsd, Hep> Server<F, Tm, R, Sub, H, Hsd, Hep>
where
    F: TransportFactory + 'static,
    F::Socket: 'static,
    Tm: Timer + Clone + 'static,
    R: E2ERegistryHandle,
    Sub: SubscriptionHandle,
    H: SharedHandle<F::Socket>,
    Hsd: SharedHandle<SdStateManager>,
    Hep: SharedHandle<EventPublisher<R, Sub, H, F::Socket>>,
{
    /// Construct a `Server` from pre-built dependencies + storage
    /// handles. The bare-metal-no-alloc counterpart to
    /// [`Self::new_with_deps`].
    ///
    /// Unlike `new_with_deps`, this constructor does NOT call
    /// `factory.bind(...)` and does NOT join any multicast group.
    /// The caller has already bound their unicast and SD sockets
    /// (typically against an externally-managed UDP stack — lwIP,
    /// vendor IP, etc.) and joined the SOME/IP-SD multicast group
    /// (`224.0.23.0`) on the SD socket externally. The caller has
    /// also assembled the `EventPublisher` and `SdStateManager`
    /// handles into whatever shared-storage their target uses
    /// (`Arc<...>` on alloc, `&'static ...` on no-alloc).
    ///
    /// `config.local_port` is back-filled from
    /// `unicast_socket.local_addr()?.port()` *only when the caller
    /// passed `local_port = 0`*. If the caller supplied a non-zero
    /// `local_port`, it must equal the actual bound port — otherwise
    /// the SD offers would advertise a port the unicast socket isn't
    /// listening on. This matches `Server::new_with_deps`'s
    /// back-fill-only-on-zero discipline.
    ///
    /// # Errors
    ///
    /// Returns an error if querying `unicast_socket.local_addr()`
    /// fails on the underlying transport, or
    /// [`Error::InvalidUsage`] if `config.local_port` is non-zero
    /// and does not equal the unicast socket's bound port.
    pub fn new_with_handles(
        deps: ServerStorage<F, Tm, R, Sub, H, Hsd, Hep>,
        mut config: ServerConfig,
    ) -> Result<Self, Error> {
        let bound_port = deps.unicast_socket.get().local_addr()?.port();
        if config.local_port == 0 {
            config.local_port = bound_port;
        } else if config.local_port != bound_port {
            crate::log::error!(
                "ServerConfig.local_port ({}) does not match unicast socket's \
                 bound port ({}); SD offers would lie. Pass local_port = 0 to \
                 auto-fill from the bound port instead.",
                config.local_port,
                bound_port,
            );
            return Err(Error::InvalidUsage("new_with_handles_local_port_mismatch"));
        }
        crate::log::info!(
            "Server (handles) bound to {}:{} for service 0x{:04X}",
            config.interface,
            bound_port,
            config.service_id
        );

        Ok(Self {
            config,
            unicast_socket: deps.unicast_socket,
            sd_socket: deps.sd_socket,
            subscriptions: deps.subscriptions,
            publisher: deps.publisher,
            sd_state: deps.sd_state,
            e2e_registry: deps.e2e_registry,
            factory: deps.factory,
            timer: deps.timer,
            is_passive: false,
            started: deps.started,
            non_sd_observer: deps.non_sd_observer,
        })
    }

    /// Passive-server counterpart to [`Self::new_with_handles`].
    ///
    /// Same shape; the resulting server is marked
    /// `is_passive = true` so [`Self::announcement_loop`] /
    /// [`Self::announcement_loop_local`] / [`Self::run`] /
    /// [`Self::run_with_buffers`] return
    /// `Err(Error::InvalidUsage(...))` rather than driving the SD
    /// loop. The caller is expected to handle SD externally
    /// (typically via a `Client::sd_announcements_loop` on the
    /// same host).
    ///
    /// The `sd_socket` field is retained but never driven; pass
    /// any pre-built handle the caller can spare (a placeholder
    /// socket bound to an ephemeral port is fine, mirroring
    /// `Server::new_passive_with_deps`).
    ///
    /// # Errors
    ///
    /// Returns an error if querying `unicast_socket.local_addr()`
    /// fails on the underlying transport, or
    /// [`Error::InvalidUsage`] if `config.local_port` is non-zero
    /// and does not equal the unicast socket's bound port (same
    /// back-fill-only-on-zero discipline as
    /// [`Self::new_with_handles`]).
    pub fn new_passive_with_handles(
        deps: ServerStorage<F, Tm, R, Sub, H, Hsd, Hep>,
        mut config: ServerConfig,
    ) -> Result<Self, Error> {
        let bound_port = deps.unicast_socket.get().local_addr()?.port();
        if config.local_port == 0 {
            config.local_port = bound_port;
        } else if config.local_port != bound_port {
            crate::log::error!(
                "ServerConfig.local_port ({}) does not match unicast socket's \
                 bound port ({}); event publishers would advertise a port \
                 nothing is listening on. Pass local_port = 0 to auto-fill.",
                config.local_port,
                bound_port,
            );
            return Err(Error::InvalidUsage(
                "new_passive_with_handles_local_port_mismatch",
            ));
        }
        crate::log::info!(
            "Passive server (handles) bound to {}:{} for service 0x{:04X}",
            config.interface,
            bound_port,
            config.service_id
        );

        Ok(Self {
            config,
            unicast_socket: deps.unicast_socket,
            sd_socket: deps.sd_socket,
            subscriptions: deps.subscriptions,
            publisher: deps.publisher,
            sd_state: deps.sd_state,
            e2e_registry: deps.e2e_registry,
            factory: deps.factory,
            timer: deps.timer,
            is_passive: true,
            started: deps.started,
            non_sd_observer: deps.non_sd_observer,
        })
    }

    /// Get a clone of the event-publisher handle for sending events.
    ///
    /// Returns the `Hep` type parameter — typically
    /// `Arc<EventPublisher<R, Sub, H, T>>` for std users (the default
    /// `Hep`), `&'static EventPublisher<R, Sub, H, T>` for
    /// bare-metal-no-alloc. (`EventPublisherHandle` was a former
    /// trait alias collapsed into [`crate::transport::SharedHandle`].)
    #[must_use]
    pub fn publisher(&self) -> Hep {
        self.publisher.clone()
    }

    /// Get the local address of the unicast socket.
    ///
    /// # Errors
    ///
    /// Returns an error if the socket's local address cannot be retrieved.
    pub fn unicast_local_addr(&self) -> Result<core::net::SocketAddr, Error> {
        match self.unicast_socket.get().local_addr() {
            Ok(v4) => Ok(core::net::SocketAddr::V4(v4)),
            Err(e) => Err(Error::Transport(e)),
        }
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
    /// Drives the receive loop (handling incoming `Subscribe` /
    /// `FindService` SD messages on the SD multicast socket and
    /// unicast traffic on the unicast socket) concurrently with the
    /// 1-Hz `OfferService` announcement loop. The two are combined
    /// into a single future so callers cannot forget to spawn the
    /// announcement side; passing
    /// [`ServerConfig::with_announce(false)`] suppresses the
    /// announcement arm for dispatcher topologies where a co-located
    /// `Client` drives SD on the server's behalf.
    ///
    /// `unicast_buf` and `sd_buf` are caller-supplied scratch buffers
    /// for incoming datagrams. Each must be at least one MTU
    /// (~1500 bytes) and ideally up to the IP datagram limit
    /// (64 KiB - 1). On bare-metal targets, callers typically place
    /// these in `static` storage; on std (or any alloc-using
    /// target), [`Self::run`] is the convenience shim that
    /// heap-allocates 64 KiB buffers and delegates here.
    ///
    /// The returned future is independent of `&self` — the cheap
    /// shared-handle clones it captures own everything it needs to
    /// drive both loops, so the caller can keep using `Server` to
    /// register E2E profiles, query `unicast_local_addr`, etc. while
    /// the future runs.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidUsage`] (tag `"passive_server_run"`) if
    /// the server was constructed via `Server::new_passive*` — passive
    /// servers have no real SD socket to read from, so the run loop
    /// would block forever on the ephemeral placeholder socket.
    ///
    /// Otherwise resolves to `Err` if receiving from a socket fails or
    /// handling an SD message fails.
    pub fn run_with_buffers<'a>(
        &self,
        unicast_buf: &'a mut [u8],
        sd_buf: &'a mut [u8],
    ) -> impl core::future::Future<Output = Result<(), Error>> + 'a + use<'a, F, Tm, R, Sub, H, Hsd, Hep>
    where
        Tm: 'a,
        Sub: 'a,
        H: 'a,
        Hsd: 'a,
    {
        let config = self.config.clone();
        let unicast_socket = self.unicast_socket.clone();
        let sd_socket = self.sd_socket.clone();
        let subscriptions = self.subscriptions.clone();
        let sd_state = self.sd_state.clone();
        let timer = self.timer.clone();
        let is_passive = self.is_passive;
        let non_sd_observer = self.non_sd_observer;
        #[allow(noop_method_call)]
        let started = self.started.clone();

        async move {
            // See `run_inner` for the rationale on the first-poll
            // latch — same race, same fix.
            if started
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                crate::log::warn!(
                    "Server::run_with_buffers already started for service 0x{:04X}; \
                     a second run-future cannot share the same sockets \
                     and session counter",
                    config.service_id
                );
                return Err(Error::InvalidUsage("server_already_running"));
            }

            runtime::run_combined::<H, F::Socket, Sub, Hsd, Tm>(
                config,
                unicast_socket,
                sd_socket,
                subscriptions,
                sd_state,
                timer,
                is_passive,
                unicast_buf,
                sd_buf,
                non_sd_observer,
            )
            .await
        }
    }

    /// Run *only* the SD `OfferService` announcement loop, without
    /// driving the receive path. Use this on supplementary Servers
    /// that share a `sd_socket` / `unicast_socket` handle (via
    /// [`Self::new_with_handles`]) with a primary Server already
    /// running [`Self::run_with_buffers`]: the primary owns the
    /// inbound recv loops, supplementary Servers add their own
    /// `OfferService` to the same SD multicast group without
    /// competing for inbound datagrams.
    ///
    /// Design note: this partially reintroduces the split-future shape
    /// phase 21 removed — deliberately. An announce-only future never
    /// touches the receive path, so the invariant that motivated the
    /// phase-21 combined run-future (no two futures racing the same
    /// sockets and SD session counter) is preserved: the [`Self::run`]
    /// path is still guarded by the first-poll `started` latch, and
    /// supplementary announce loops only ever *send* on the shared SD
    /// socket.
    ///
    /// The returned future loops forever (1 s tick between
    /// announcements); spawn it on your executor.
    pub fn announce_only_future<'a>(
        &self,
    ) -> impl core::future::Future<Output = ()> + 'a + use<'a, F, Tm, R, Sub, H, Hsd, Hep>
    where
        Tm: 'a,
        Hsd: 'a,
        H: 'a,
    {
        let config = self.config.clone();
        let sd_socket = self.sd_socket.clone();
        let sd_state = self.sd_state.clone();
        let timer = self.timer.clone();
        async move {
            runtime::announce_loop(&config, sd_socket.get(), sd_state.get(), &timer).await;
        }
    }

    /// Run the server event loop with heap-allocated 64 KiB receive
    /// buffers — the convenience entry point for std and alloc-using
    /// bare-metal builds. Drives both the receive loop and (unless
    /// suppressed via [`ServerConfig::with_announce`]) the
    /// announcement loop in a single future.
    ///
    /// The returned future is `Send + 'static` under the where-clause
    /// bounds spelled below, so it is suitable for `tokio::spawn`.
    /// Single-threaded executors that need a `!Send` future (e.g.
    /// `tokio::task::spawn_local` over a `!Sync` transport) should
    /// call [`Self::run_with_buffers`] directly, which has no `Send`
    /// requirement.
    ///
    /// Bare-metal callers without an allocator must use
    /// [`Self::run_with_buffers`] with caller-supplied buffers
    /// (e.g. `static`-declared `[u8; N]` arrays).
    ///
    /// # Errors
    ///
    /// Same as [`Self::run_with_buffers`].
    #[cfg(feature = "_alloc")]
    pub fn run(
        &self,
    ) -> impl core::future::Future<Output = Result<(), Error>>
    + Send
    + 'static
    + use<F, Tm, R, Sub, H, Hsd, Hep>
    where
        F: Send + Sync,
        F::Socket: Send + Sync,
        for<'a> <F::Socket as TransportSocket>::SendFuture<'a>: Send,
        for<'a> <F::Socket as TransportSocket>::RecvFuture<'a>: Send,
        H: Send + Sync,
        Sub: Send + Sync,
        for<'a> Sub::SubscribeFuture<'a>: Send,
        for<'a> Sub::UnsubscribeFuture<'a>: Send,
        R: Send + Sync,
        Tm: Send + Sync,
        for<'a> Tm::SleepFuture<'a>: Send,
        Hsd: Send + Sync,
        Hep: Send + Sync,
    {
        self.run_inner()
    }

    /// Auto-trait-inferred run-future used by the constructors and by
    /// the `Send`-requiring [`Self::run`] convenience above. Private
    /// because it exposes `Send`-or-not as an inference rather than a
    /// declared bound — callers should prefer `run` (Send-checked at
    /// the API boundary) or `run_with_buffers` (explicitly no `Send`
    /// requirement).
    #[cfg(feature = "_alloc")]
    fn run_inner(
        &self,
    ) -> impl core::future::Future<Output = Result<(), Error>> + 'static + use<F, Tm, R, Sub, H, Hsd, Hep>
    {
        let config = self.config.clone();
        let unicast_socket = self.unicast_socket.clone();
        let sd_socket = self.sd_socket.clone();
        let subscriptions = self.subscriptions.clone();
        let sd_state = self.sd_state.clone();
        let timer = self.timer.clone();
        let is_passive = self.is_passive;
        let non_sd_observer = self.non_sd_observer;
        let started = self.started.clone();

        async move {
            // First-poll latch — guards against a caller spawning
            // both the constructor's run-future *and* a fresh
            // `server.run()` / `server.run_with_buffers()`. Two
            // concurrent receive loops would race on the same SD /
            // unicast sockets and the SD session counter; reject the
            // second one rather than silently corrupt wire output.
            if started
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                crate::log::warn!(
                    "Server::run already started for service 0x{:04X}; \
                     a second run-future cannot share the same sockets \
                     and session counter",
                    config.service_id
                );
                return Err(Error::InvalidUsage("server_already_running"));
            }

            let mut unicast_buf = alloc::vec![0u8; 65535];
            let mut sd_buf = alloc::vec![0u8; 65535];
            runtime::run_combined::<H, F::Socket, Sub, Hsd, Tm>(
                config,
                unicast_socket,
                sd_socket,
                subscriptions,
                sd_state,
                timer,
                is_passive,
                &mut unicast_buf,
                &mut sd_buf,
                non_sd_observer,
            )
            .await
        }
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
        TokioTransport,
        TokioTimer,
        Arc<Mutex<E2ERegistry>>,
        Arc<RwLock<SubscriptionManager>>,
    >;

    #[tokio::test]
    async fn test_server_creation() {
        let config = ServerConfig::new(0x5B, 1)
            .with_interface(Ipv4Addr::LOCALHOST)
            .with_local_port(30682);

        let result = TestServer::new(config).await;
        assert!(result.is_ok());
    }

    #[test]
    fn server_config_builder_chain_overrides_each_field() {
        let cfg = ServerConfig::new(0x5B, 1)
            .with_interface(Ipv4Addr::LOCALHOST)
            .with_local_port(30683)
            .with_major_version(2)
            .with_minor_version(7)
            .with_ttl(core::time::Duration::from_secs(10))
            .with_event_group(0x42)
            .with_event_group(0x43);
        assert_eq!(cfg.interface, Ipv4Addr::LOCALHOST);
        assert_eq!(cfg.local_port, 30683);
        assert_eq!(cfg.major_version, 2);
        assert_eq!(cfg.minor_version, 7);
        assert_eq!(cfg.ttl, 10);
        assert!(cfg.accepts_event_group(0x42));
        assert!(cfg.accepts_event_group(0x43));
        assert!(!cfg.accepts_event_group(0x44));
    }

    #[test]
    fn server_config_with_ttl_truncates_subsecond_precision() {
        let cfg = ServerConfig::new(0x5B, 1).with_ttl(core::time::Duration::from_millis(2_999));
        assert_eq!(cfg.ttl, 2, "sub-second is truncated, not rounded");
    }

    /// `announce` defaults to `true` from `ServerConfig::new`, and
    /// `with_announce(false)` flips it. The dispatcher topology in
    /// `examples/client_server` depends on this default-vs-override
    /// being load-bearing — see
    /// `with_announce_false_suppresses_offer_service` for the
    /// behavioral counterpart that proves the run-future actually
    /// honours the flag.
    #[test]
    fn server_config_with_announce_toggles_field() {
        let default_cfg = ServerConfig::new(0x5B, 1);
        assert!(
            default_cfg.announce,
            "announce must default to true so a fresh `ServerConfig` emits SD offers"
        );

        let suppressed = default_cfg.clone().with_announce(false);
        assert!(
            !suppressed.announce,
            "with_announce(false) must clear the field"
        );

        let restored = suppressed.with_announce(true);
        assert!(
            restored.announce,
            "with_announce(true) must re-enable after a previous suppression"
        );
    }

    #[test]
    fn server_config_with_ttl_saturates_overflow() {
        let cfg = ServerConfig::new(0x5B, 1)
            .with_ttl(core::time::Duration::from_secs(u64::from(u32::MAX) + 1));
        assert_eq!(cfg.ttl, u32::MAX);
    }

    #[test]
    fn server_config_try_with_event_group_rejects_at_capacity() {
        let mut cfg = ServerConfig::new(0x5B, 1)
            .with_interface(Ipv4Addr::LOCALHOST)
            .with_local_port(30684);
        for i in 0..u16::try_from(ServerConfig::EVENT_GROUP_IDS_CAP).unwrap() {
            cfg = cfg.try_with_event_group(i).expect("under cap");
        }
        // One more should be rejected and return the unmodified config.
        let cap = ServerConfig::EVENT_GROUP_IDS_CAP;
        let result = cfg.try_with_event_group(0xFFFF);
        let returned = result.expect_err("at-cap insert must fail");
        assert_eq!(returned.event_group_ids.len(), cap);
        assert!(!returned.accepts_event_group(0xFFFF));
    }

    // ── new_with_handles / new_passive_with_handles tests ──────────────
    //
    // These constructors take pre-built socket handles instead of
    // calling `factory.bind()` themselves, and validate that the
    // caller-supplied `config.local_port` matches the actual bound
    // port (back-fill-only-on-zero). The validation logic only
    // exercises through these tests; the production code paths use
    // `new` / `new_with_deps`.

    /// Build a `ServerStorage<…>` whose unicast socket is bound to
    /// the given port (port `0` for ephemeral) and whose other
    /// fields are the std defaults a tokio consumer would assemble.
    /// Used by the `new_with_handles` tests below.
    async fn build_test_handles(
        unicast_port: u16,
    ) -> (
        ServerStorage<
            TokioTransport,
            TokioTimer,
            Arc<Mutex<E2ERegistry>>,
            Arc<RwLock<SubscriptionManager>>,
            Arc<crate::tokio_transport::TokioSocket>,
            Arc<SdStateManager>,
            Arc<
                EventPublisher<
                    Arc<Mutex<E2ERegistry>>,
                    Arc<RwLock<SubscriptionManager>>,
                    Arc<crate::tokio_transport::TokioSocket>,
                    crate::tokio_transport::TokioSocket,
                >,
            >,
        >,
        u16, // actual bound port (0 → ephemeral)
    ) {
        let factory = TokioTransport;
        let unicast_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, unicast_port);
        let unicast_raw = factory
            .bind(unicast_addr, &SocketOptions::new())
            .await
            .expect("bind unicast");
        let bound_port = unicast_raw.local_addr().expect("local_addr").port();
        let unicast_socket = Arc::new(unicast_raw);
        // SD socket is bound ephemerally — these tests don't drive
        // `run_with_buffers` so the SD socket never has to be on
        // 30490 / multicast-joined.
        let sd_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
        let sd_socket = Arc::new(
            factory
                .bind(sd_addr, &SocketOptions::new())
                .await
                .expect("bind sd"),
        );
        let e2e_registry = Arc::new(Mutex::new(E2ERegistry::new()));
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let publisher = Arc::new(EventPublisher::new(
            subscriptions.clone(),
            unicast_socket.clone(),
            e2e_registry.clone(),
        ));
        let handles = ServerStorage {
            factory,
            timer: TokioTimer,
            e2e_registry,
            subscriptions,
            unicast_socket,
            sd_socket,
            sd_state: Arc::new(SdStateManager::new()),
            publisher,
            started: Arc::new(AtomicBool::new(false)),
            non_sd_observer: None,
        };
        (handles, bound_port)
    }

    #[tokio::test]
    async fn new_with_handles_back_fills_local_port_on_zero() {
        let (handles, bound_port) = build_test_handles(0).await;
        assert_ne!(
            bound_port, 0,
            "test precondition: kernel must assign a real ephemeral port",
        );
        // Port 0 → caller asks for back-fill from the bound port.
        let config = ServerConfig::new(0xFE10, 1)
            .with_interface(Ipv4Addr::LOCALHOST)
            .with_local_port(0);
        let server = TestServer::new_with_handles(handles, config)
            .expect("new_with_handles must accept local_port = 0");
        assert_eq!(
            server.config.local_port, bound_port,
            "config.local_port must be back-filled from the unicast socket's bound port",
        );
    }

    #[tokio::test]
    async fn new_with_handles_accepts_matching_local_port() {
        let (handles, bound_port) = build_test_handles(0).await;
        // Caller supplies the matching port explicitly.
        let config = ServerConfig::new(0xFE11, 1)
            .with_interface(Ipv4Addr::LOCALHOST)
            .with_local_port(bound_port);
        let server = TestServer::new_with_handles(handles, config)
            .expect("matching local_port must be accepted");
        assert_eq!(server.config.local_port, bound_port);
    }

    #[tokio::test]
    async fn new_with_handles_rejects_local_port_mismatch() {
        let (handles, bound_port) = build_test_handles(0).await;
        // Bogus port: deterministically `bound_port + 1` (wrapping
        // for the impossible bound_port == u16::MAX). The kernel
        // doesn't allocate adjacent ports back-to-back across separate
        // bind() calls in the same process, so this is reliably
        // distinct from `bound_port`.
        let bogus_port = bound_port.wrapping_add(1);
        assert_ne!(bogus_port, bound_port);
        let config = ServerConfig::new(0xFE12, 1)
            .with_interface(Ipv4Addr::LOCALHOST)
            .with_local_port(bogus_port);
        let result = TestServer::new_with_handles(handles, config);
        match result {
            Err(Error::InvalidUsage(tag)) => {
                assert_eq!(tag, "new_with_handles_local_port_mismatch");
            }
            Ok(_) => panic!("non-zero non-matching local_port must be rejected"),
            Err(other) => {
                panic!(
                    "expected Error::InvalidUsage(\"new_with_handles_local_port_mismatch\"), got {other:?}"
                )
            }
        }
    }

    #[tokio::test]
    async fn new_passive_with_handles_back_fills_local_port_on_zero() {
        let (handles, bound_port) = build_test_handles(0).await;
        let config = ServerConfig::new(0xFE13, 1)
            .with_interface(Ipv4Addr::LOCALHOST)
            .with_local_port(0);
        let server = TestServer::new_passive_with_handles(handles, config)
            .expect("new_passive_with_handles must accept local_port = 0");
        assert_eq!(server.config.local_port, bound_port);
        assert!(server.is_passive, "passive constructor must set is_passive");
    }

    #[tokio::test]
    async fn new_passive_with_handles_rejects_local_port_mismatch() {
        let (handles, bound_port) = build_test_handles(0).await;
        let bogus_port = bound_port.wrapping_add(1);
        assert_ne!(bogus_port, bound_port);
        let config = ServerConfig::new(0xFE14, 1)
            .with_interface(Ipv4Addr::LOCALHOST)
            .with_local_port(bogus_port);
        let result = TestServer::new_passive_with_handles(handles, config);
        match result {
            Err(Error::InvalidUsage(tag)) => {
                assert_eq!(tag, "new_passive_with_handles_local_port_mismatch");
            }
            Ok(_) => panic!("non-zero non-matching local_port must be rejected"),
            Err(other) => panic!("unexpected: {other:?}"),
        }
    }

    /// Passive server's `run_with_buffers` must short-circuit with
    /// `Err(InvalidUsage)` rather than block forever on the
    /// ephemeral SD socket.
    #[tokio::test]
    async fn passive_server_run_with_buffers_returns_invalid_usage() {
        let (handles, _) = build_test_handles(0).await;
        let config = ServerConfig::new(0xFE15, 1)
            .with_interface(Ipv4Addr::LOCALHOST)
            .with_local_port(0);
        let server = TestServer::new_passive_with_handles(handles, config).expect("passive ctor");
        let mut unicast_buf = vec![0u8; 1500];
        let mut sd_buf = vec![0u8; 1500];
        let result = server.run_with_buffers(&mut unicast_buf, &mut sd_buf).await;
        match result {
            Err(Error::InvalidUsage(tag)) => assert_eq!(tag, "passive_server_run"),
            other => {
                panic!("passive server's run_with_buffers must return InvalidUsage, got {other:?}",)
            }
        }
    }

    // No standalone `passive_server_announcement_loop` test: the
    // announcement loop is folded into the combined [`Server::run`]
    // future, so the only entry point that can short-circuit on a
    // passive server is `run_with_buffers` (covered by
    // `passive_server_run_with_buffers_returns_invalid_usage` above).

    /// Regression for H5: `ServerConfig::accepts_event_group` must
    /// accept any group when `event_group_ids` is empty (back-compat:
    /// servers that have not enumerated their groups must keep
    /// working) and validate strictly when populated.
    #[test]
    fn server_config_accepts_event_group_empty_means_any() {
        let config = ServerConfig::new(0x5B, 1)
            .with_interface(Ipv4Addr::LOCALHOST)
            .with_local_port(30490);
        assert!(config.event_group_ids.is_empty());
        // Empty list: every group accepted.
        assert!(config.accepts_event_group(0x0001));
        assert!(config.accepts_event_group(0xBEEF));
        assert!(config.accepts_event_group(0xFFFF));
    }

    #[test]
    fn server_config_accepts_event_group_populated_validates() {
        let mut config = ServerConfig::new(0x5B, 1)
            .with_interface(Ipv4Addr::LOCALHOST)
            .with_local_port(30490);
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
            non_sd_observer: None,
        };
        let config = ServerConfig::new(0x5B, 1)
            .with_interface(Ipv4Addr::LOCALHOST)
            .with_local_port(0);
        // Explicit `Arc<FailingSocket>` H so the compiler doesn't have
        // to invent it across the deps-bundle indirection.
        let (server, _handles, _run): (Server<_, _, _, _, Arc<FailingSocket>>, _, _) =
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
        let result = runtime::handle_sd_message(
            &server.config,
            server.sd_socket.get(),
            server.sd_state.get(),
            &server.subscriptions,
            &sd_view,
            sender,
        )
        .await;
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

    // No standalone `announcement_loop` method: the announcement
    // loop is folded into the single combined run-future, so there
    // is only one entry point. (The previous
    // `announcement_loop_started: AtomicBool` latch existed because
    // two independently-spawned announcement futures would race on
    // the SD socket / session counter; that failure mode is now
    // structurally impossible.)

    #[tokio::test]
    async fn test_server_creation_with_loopback_enabled() {
        // Use a unicast port distinct from other tests to avoid EADDRINUSE
        // when the test binary runs tests in parallel. The SD socket binds
        // the SD multicast port (30490) and relies on SO_REUSEPORT, the same
        // as `test_server_creation`.
        let config = ServerConfig::new(0x5C, 1)
            .with_interface(Ipv4Addr::LOCALHOST)
            .with_local_port(30683);

        let (server, _handles, _run) = TestServer::new_with_loopback(config, true)
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
        let config = ServerConfig::new(service_id, instance_id)
            .with_interface(Ipv4Addr::LOCALHOST)
            .with_local_port(0);
        let (server, _handles, _run) = TestServer::new(config)
            .await
            .expect("Failed to create server");
        // Constructor already back-filled `config.local_port` from the
        // kernel-assigned bound port; just read it back via
        // `unicast_local_addr` for the test return.
        let port = match server.unicast_local_addr().unwrap() {
            core::net::SocketAddr::V4(addr) => addr.port(),
            core::net::SocketAddr::V6(_) => panic!("expected IPv4 address"),
        };
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
        let (server, server_port) = create_test_server(0x5B, 1).await;

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
            runtime::handle_sd_message(
                &server.config,
                server.sd_socket.get(),
                server.sd_state.get(),
                &server.subscriptions,
                &sd_view,
                addr,
            )
            .await
            .unwrap();

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
        let (server, server_port) = create_test_server(0x5B, 1).await;
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
            runtime::handle_sd_message(
                &server.config,
                server.sd_socket.get(),
                server.sd_state.get(),
                &server.subscriptions,
                &sd_view,
                addr,
            )
            .await
            .unwrap();

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
        let (server, server_port) = create_test_server(0x5B, 1).await;
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
            runtime::handle_sd_message(
                &server.config,
                server.sd_socket.get(),
                server.sd_state.get(),
                &server.subscriptions,
                &sd_view,
                addr,
            )
            .await
            .unwrap();

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
        let (server, server_port) = create_test_server(0x5B, 1).await;
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
            runtime::handle_sd_message(
                &server.config,
                server.sd_socket.get(),
                server.sd_state.get(),
                &server.subscriptions,
                &sd_view,
                addr,
            )
            .await
            .unwrap();
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
        let (server, server_port) = create_test_server(0x5B, 1).await;
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
            runtime::handle_sd_message(
                &server.config,
                server.sd_socket.get(),
                server.sd_state.get(),
                &server.subscriptions,
                &sd_view,
                addr,
            )
            .await
            .unwrap();
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
        let (server, server_port) = create_test_server(0x5B, 1).await;
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
            runtime::handle_sd_message(
                &server.config,
                server.sd_socket.get(),
                server.sd_state.get(),
                &server.subscriptions,
                &sd_view,
                addr,
            )
            .await
            .unwrap();
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
        let (server, server_port) = create_test_server(0x5B, 1).await;
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
            runtime::handle_sd_message(
                &server.config,
                server.sd_socket.get(),
                server.sd_state.get(),
                &server.subscriptions,
                &sd_view,
                addr,
            )
            .await
            .unwrap();

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
        runtime::send_unicast_offer(
            &server.config,
            server.sd_socket.get(),
            server.sd_state.get(),
            recv_addr,
        )
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

        // Announcements are folded into `Server::run`. Verify a
        // fresh server can build its combined run-future without
        // error; intentionally do not poll or spawn it (would loop
        // indefinitely emitting multicast).
        drop(server);
        let (server2, _) = create_test_server(0x5B, 1).await;
        let fut = server2.run();
        drop(fut);
    }

    #[tokio::test]
    async fn test_run_non_sd_message() {
        let (server, server_port) = create_test_server(0x5B, 1).await;
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
        let (server, server_port) = create_test_server(0x5B, 1).await;
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
        let (server, _) = create_test_server(0x5B, 1).await;

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
        let result = runtime::handle_sd_message(
            &server.config,
            server.sd_socket.get(),
            server.sd_state.get(),
            &server.subscriptions,
            &sd_view,
            "127.0.0.1:12345".parse().unwrap(),
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_subscribe_ack_different_endpoint_port() {
        let (server, server_port) = create_test_server(0x5B, 1).await;
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
            runtime::handle_sd_message(
                &server.config,
                server.sd_socket.get(),
                server.sd_state.get(),
                &server.subscriptions,
                &sd_view,
                addr,
            )
            .await
            .unwrap();

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

        let got = runtime::extract_subscriber_endpoint(&iter, 0, 1, 0, 0);
        assert_eq!(
            got,
            Some(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 30000))
        );
    }

    #[test]
    fn extract_endpoint_zero_options_in_both_runs_returns_none() {
        let iter = sd::OptionIter::new(&[]);
        assert_eq!(
            runtime::extract_subscriber_endpoint(&iter, 0, 0, 0, 0),
            None
        );
    }

    #[test]
    fn extract_endpoint_count_zero_with_nonzero_index_returns_none() {
        // An entry with first_count = 0 at a non-zero index must not
        // dereference anything, even if the options array has data past
        // that index.
        let mut buf = [0u8; 64];
        let total = fill_ipv4_endpoints(&mut buf, 2, 30100);
        let iter = sd::OptionIter::new(&buf[..total]);

        assert_eq!(
            runtime::extract_subscriber_endpoint(&iter, 1, 0, 0, 0),
            None
        );
    }

    #[test]
    fn extract_endpoint_multi_option_first_run_returns_first() {
        // Two IpV4Endpoint options in the first run. The helper should
        // return the first and log a warning about the second. We just
        // verify the return value here.
        let mut buf = [0u8; 64];
        let total = fill_ipv4_endpoints(&mut buf, 2, 30200);
        let iter = sd::OptionIter::new(&buf[..total]);

        let got = runtime::extract_subscriber_endpoint(&iter, 0, 2, 0, 0);
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

        let got = runtime::extract_subscriber_endpoint(&iter, 0, 1, 2, 1);
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

        let got = runtime::extract_subscriber_endpoint(&iter, 2, 1, 0, 0);
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
        let got = runtime::extract_subscriber_endpoint(&iter, 1, 1, 0, 0);
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

        let got = runtime::extract_subscriber_endpoint(&iter, 0, 3, 0, 0);
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

        assert_eq!(
            runtime::extract_subscriber_endpoint(&iter, 0, 2, 0, 0),
            None
        );
    }

    #[test]
    fn extract_endpoint_second_run_only() {
        // Two options, entry references only the second one via the
        // second_options_run pair.
        let mut buf = [0u8; 64];
        let total = fill_ipv4_endpoints(&mut buf, 2, 30700);
        let iter = sd::OptionIter::new(&buf[..total]);

        let got = runtime::extract_subscriber_endpoint(&iter, 0, 0, 1, 1);
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
        let (server, _port) = create_test_server(0x5B, 1).await;

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
        runtime::handle_sd_message(
            &server.config,
            server.sd_socket.get(),
            server.sd_state.get(),
            &server.subscriptions,
            &sd_view,
            sender,
        )
        .await
        .unwrap();

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
        let config = ServerConfig::new(service_id, instance_id)
            .with_interface(Ipv4Addr::LOCALHOST)
            .with_local_port(0);
        let (server, _handles, _run) = TestServer::new_passive(config)
            .await
            .expect("new_passive should succeed");
        server
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

    // The announcement loop is folded into the combined
    // `Server::run` future, so the `is_passive` check happens on
    // `run` itself — exercised by
    // `run_on_passive_returns_invalid_input` below.

    #[tokio::test]
    async fn run_on_passive_returns_invalid_input() {
        let server = make_passive_server(0x005C, 0x0001).await;
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
    async fn run_on_regular_server_builds_future_ok() {
        // Regression guard: the combined run-future must build
        // without error on a non-passive server. We don't poll or
        // spawn — doing so would leave the run-loop emitting
        // multicast for the rest of the test binary's lifetime and
        // interfere with parallel tests that share the SD multicast
        // group.
        let (server, _port) = create_test_server(0x005C, 0x0001).await;
        let fut = server.run();
        drop(fut);
    }

    /// Two run-futures from the same `Server` would race on the SD
    /// and unicast sockets and the SD session counter; the second to
    /// be polled must short-circuit with
    /// `Err(Error::InvalidUsage("server_already_running"))` rather
    /// than silently corrupt wire output. Tests both ordering and
    /// the buffer-supplied variant.
    #[tokio::test]
    async fn second_run_future_returns_already_running() {
        let (server, _port) = create_test_server(0x005D, 0x0001).await;

        // First run-future: spawn it so its async-move body actually
        // runs and flips the latch on first poll. Yield once so tokio
        // schedules the spawned task; the task itself blocks
        // indefinitely in `recv_from`, which is fine — abort below.
        let first = tokio::spawn(server.run());
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        // Second run-future from the same server must reject.
        let second = server.run().await;
        match second {
            Err(Error::InvalidUsage(tag)) => {
                assert_eq!(tag, "server_already_running");
            }
            other => panic!(
                "second run-future must return InvalidUsage(\"server_already_running\"), got {other:?}"
            ),
        }

        // Same gate on `run_with_buffers`.
        let mut unicast_buf = vec![0u8; 1500];
        let mut sd_buf = vec![0u8; 1500];
        let third = server.run_with_buffers(&mut unicast_buf, &mut sd_buf).await;
        match third {
            Err(Error::InvalidUsage(tag)) => {
                assert_eq!(tag, "server_already_running");
            }
            other => panic!(
                "second run_with_buffers must return InvalidUsage(\"server_already_running\"), got {other:?}"
            ),
        }

        first.abort();
        let _ = first.await;
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

        let config = ServerConfig::new(SID, IID)
            .with_interface(iface)
            .with_local_port(30501);
        let (_server, _handles, run) = TestServer::new_with_loopback(config, true).await.unwrap();
        // `Server::run` is the combined receive+announce future. The
        // receive arm here just waits for traffic that never arrives
        // in this test; the announce arm is what we capture on `recv`
        // below.
        let handle = tokio::spawn(async move {
            let _ = run.await;
        });

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

    /// `ServerConfig::with_announce(false)` is the contract the
    /// dispatcher topology relies on (`examples/client_server`). It
    /// MUST suppress the announce arm of the combined run-future,
    /// even though the receive arm keeps running. This is the
    /// negative counterpart to
    /// `announcement_loop_sends_offer_service_when_driven` above —
    /// same SD-multicast capture machinery, but we assert the listen
    /// window expires *without* seeing one of our OfferServices.
    #[tokio::test]
    async fn with_announce_false_suppresses_offer_service() {
        use crate::protocol::MessageId;

        // Distinct (sid, iid) so parallel tests on the same SD multicast
        // group don't bleed into our negative assertion. These IDs must
        // not appear in any other in-tree test or example.
        const SID: u16 = 0xAA02;
        const IID: u16 = 0xFF02;

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

        let config = ServerConfig::new(SID, IID)
            .with_interface(iface)
            .with_local_port(30502)
            .with_announce(false);
        let (_server, _handles, run) = TestServer::new_with_loopback(config, true).await.unwrap();
        let handle = tokio::spawn(async move {
            let _ = run.await;
        });

        // Listen for ~2 seconds — comfortably more than the 1-second
        // announcement period the run-future would emit at if announce
        // were on. If we see an OfferService for OUR (SID, IID) in this
        // window, the suppression is broken. Stray traffic for *other*
        // service IDs is ignored (parallel tests share the SD group).
        let saw_our_offer = tokio::time::timeout(std::time::Duration::from_millis(2_500), async {
            let mut buf = [0u8; 1500];
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
                if entry.service_id() == SID && entry.instance_id() == IID {
                    break true;
                }
            }
        })
        .await
        .unwrap_or(false);

        handle.abort();
        let _ = handle.await;

        assert!(
            !saw_our_offer,
            "with_announce(false) must suppress OfferService emission for the configured \
             service; observed an OfferService for (sid={SID:#06x}, iid={IID:#06x}) within \
             the listen window. The dispatcher topology in examples/client_server depends \
             on this suppression."
        );
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

        let config = ServerConfig::new(0x005C, 0x0001)
            .with_interface(Ipv4Addr::LOCALHOST)
            .with_local_port(blocker_port);
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
        // Coverage helper: with no global tracing subscriber, `crate::log::info!`
        // and `crate::log::debug!` short-circuit before evaluating their
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
                runtime::extract_subscriber_endpoint(&iter_empty, 0, 0, 0, 0),
                None
            );

            // 1 endpoint → trace! "Found IPv4 endpoint" branch.
            let mut buf_one = [0u8; 32];
            let len_one = fill_ipv4_endpoints(&mut buf_one, 1, 31000);
            let iter_one = sd::OptionIter::new(&buf_one[..len_one]);
            assert_eq!(
                runtime::extract_subscriber_endpoint(&iter_one, 0, 1, 0, 0),
                Some(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 31000))
            );

            // n endpoints → warn! "{} IPv4 endpoints found" branch.
            let mut buf_many = [0u8; 64];
            let len_many = fill_ipv4_endpoints(&mut buf_many, 3, 31100);
            let iter_many = sd::OptionIter::new(&buf_many[..len_many]);
            assert_eq!(
                runtime::extract_subscriber_endpoint(&iter_many, 0, 3, 0, 0),
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
        let config = ServerConfig::new(service_id, 0x43)
            .with_interface(interface)
            .with_local_port(30684);

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

        let (_server, _handles, run_fut) = TestServer::new_with_loopback(config, true)
            .await
            .expect("server must bind with loopback enabled");
        // Announcement is folded into the combined run-future.
        let announce_handle = tokio::spawn(async move {
            let _ = run_fut.await;
        });

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

    /// Host-arch PROXY budget — see the twin constant in
    /// src/client/mod.rs for semantics and the update procedure.
    const TOKIO_SERVER_RUN_FUTURE_BUDGET: usize = 9728; // = ceil64(7744 × 1.25)

    #[tokio::test]
    async fn future_size_witness_tokio_server() {
        // Port 0: kernel-assigned, back-filled by the constructor —
        // avoids collisions with sibling tests running in parallel.
        let config = ServerConfig::new(0x5B, 1)
            .with_interface(Ipv4Addr::LOCALHOST)
            .with_local_port(0);
        let (_server, _handles, run) = TestServer::new(config).await.expect("Server::new");

        let run_size = core::mem::size_of_val(&run);
        std::println!("FUTURE_SIZE tokio_server_run_future {run_size}");
        assert!(
            run_size <= TOKIO_SERVER_RUN_FUTURE_BUDGET,
            "server run future grew: {run_size} B > budget {TOKIO_SERVER_RUN_FUTURE_BUDGET} B"
        );
    }
}
