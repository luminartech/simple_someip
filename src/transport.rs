//! Executor-agnostic transport abstraction.
//!
//! [`TransportSocket`] is the minimum UDP surface `simple-someip` needs from
//! its networking backend: unicast and multicast send/recv plus a few
//! socket-level knobs. [`TransportFactory`] constructs bound and configured
//! sockets at startup. [`Timer`] provides async sleep.
//!
//! # Why a trait, and why like this
//!
//! The crate's `client` and `server` modules today use a tokio-based UDP
//! backend, with sockets created/configured via `socket2` (for reuse /
//! multicast-interface / multicast-loop options) and then handed off as
//! `tokio::net::UdpSocket` for the async I/O loop. That works on
//! `std + tokio` but makes no-`std` / non-tokio embedded use impossible.
//! These traits are the integration point for alternative backends (lwIP,
//! smoltcp, etc.).
//!
//! Three explicit design choices:
//!
//! 1. **Executor-agnostic for socket / timer I/O.** [`TransportSocket`]
//!    and [`Timer`] methods return `impl Future`, not `async fn`, and
//!    those traits make no statement about `Send` or `'static` bounds on
//!    their returned futures. Callers that need those bounds (e.g. to
//!    `tokio::spawn`) require them at the consumer site. Bare-metal
//!    callers driving the future on a single executor task pay no `Send`
//!    tax for socket I/O. **[`Spawner::spawn`] is the deliberate
//!    exception:** it is a multi-task abstraction by definition, so it
//!    requires `Send + 'static` on its argument. Single-core executors
//!    that need a `!Send` variant (embassy with `task_arena_size = 0`,
//!    `LocalSet`-style models) need either a future `spawn_local` shim
//!    or a hand-rolled adapter; the `Send + 'static` bound is documented
//!    on the trait method itself.
//! 2. **IPv4-only address type.** This transport abstraction currently
//!    uses [`core::net::SocketAddrV4`] directly rather than `SocketAddr`,
//!    matching the crate's present transport-layer reach for unicast and
//!    the standard SD IPv4 multicast address
//!    ([`crate::protocol::sd::MULTICAST_IP`], `239.255.0.255`). This
//!    saves every backend from writing a `SocketAddr::V6(_) =>
//!    Unsupported` arm, and documents the crate's actual reach at this
//!    layer. (The protocol layer parses IPv6 SD option endpoints too;
//!    only the transport bind / send is IPv4-today.)
//! 3. **No object safety.** Because `impl Future` is used in method return
//!    positions, the traits cannot be made into trait objects
//!    (`Box<dyn TransportSocket>` will not compile). This is intentional:
//!    there is exactly one transport implementation per build, selected at
//!    compile time, and monomorphization eliminates any dispatch overhead.
//!    Consumers carry a generic `<T: TransportSocket>`.
//!
//! # `Send` and multithreaded executors
//!
//! Neither [`TransportSocket`] nor [`Timer`] method signatures require
//! their returned futures to be `Send`. This is on purpose: single-threaded
//! executors (embassy, smol's `LocalSet`, and any bare-metal task loop)
//! benefit from the relaxation and can hold `!Send` state across yield
//! points.
//!
//! Implementations targeting multithreaded executors such as `tokio::spawn`
//! are expected to produce `Send + 'static` futures in practice. Consumers
//! that require `Send` should enforce it through how they use the
//! transport, not by naming the hidden future type returned by the trait
//! methods — with RPITIT that type is anonymous and cannot be named, and
//! there is no `TransportSocketSendFut`-style associated-type escape
//! hatch here. Instead, wrap the call in an `async move` block and
//! require `T: Send + 'static` on the captured state:
//!
//! ```ignore
//! fn spawn_loop<T>(sock: T)
//! where
//!     T: TransportSocket + Send + 'static,
//! {
//!     tokio::spawn(async move {
//!         let mut sock = sock;
//!         /* use sock here */
//!     });
//! }
//! ```
//!
//! A tokio-backed implementation where the underlying `UdpSocket` is
//! already `Send + Sync` will produce `Send` futures automatically via
//! `async` block capture inference, so the pattern above works without
//! any extra trait-level future bound. Implementations that hold
//! `!Send` state internally simply won't satisfy the `T: Send` bound
//! — the compiler catches the mismatch at the `tokio::spawn` call
//! site rather than inside the trait definition.
//!
//! # Status
//!
//! A default `std + tokio` implementation
//! (`crate::tokio_transport::TokioTransport`,
//! `crate::tokio_transport::TokioSocket`, `crate::tokio_transport::TokioTimer`)
//! ships under the `client` and `server` features and is re-exported at the
//! crate root. The paths are rendered as code literals rather than
//! intra-doc links because the `tokio_transport` module is feature-gated,
//! and links would otherwise break default-feature rustdoc builds. Other
//! backends (for example `smoltcp::UdpSocket` + `embassy-time` on embedded)
//! are the consumer's responsibility — the traits here are the integration
//! point.
//!
//! # Minimal adapter sketch
//!
//! ```
//! # #[cfg(feature = "client-tokio")]
//! # fn wrapper() {
//! use core::future::Future;
//! use core::net::{Ipv4Addr, SocketAddrV4};
//! use core::time::Duration;
//! use futures::future::BoxFuture;
//! use simple_someip::transport::{
//!     IoErrorKind, ReceivedDatagram, SocketOptions, Timer, TransportError,
//!     TransportFactory, TransportSocket,
//! };
//!
//! struct TokioTransport;
//!
//! struct TokioSocket {
//!     inner: tokio::net::UdpSocket,
//! }
//!
//! impl TransportFactory for TokioTransport {
//!     type Socket = TokioSocket;
//!     fn bind(
//!         &self,
//!         addr: SocketAddrV4,
//!         _options: &SocketOptions,
//!     ) -> impl Future<Output = Result<Self::Socket, TransportError>> + Send {
//!         async move {
//!             let inner = tokio::net::UdpSocket::bind(addr)
//!                 .await
//!                 .map_err(|_| TransportError::Io(IoErrorKind::Other))?;
//!             Ok(TokioSocket { inner })
//!         }
//!     }
//! }
//!
//! impl TransportSocket for TokioSocket {
//!     // `BoxFuture` keeps this sketch short. The real `TokioSocket`
//!     // shipped under the `client` / `server` features uses named
//!     // future structs that wrap `poll_send_to` / `poll_recv_from`
//!     // for zero-allocation per datagram — see `tokio_transport.rs`.
//!     type SendFuture<'a> = BoxFuture<'a, Result<(), TransportError>>;
//!     type RecvFuture<'a> = BoxFuture<'a, Result<ReceivedDatagram, TransportError>>;
//!
//!     fn send_to<'a>(
//!         &'a self,
//!         buf: &'a [u8],
//!         target: SocketAddrV4,
//!     ) -> Self::SendFuture<'a> {
//!         Box::pin(async move {
//!             self.inner
//!                 .send_to(buf, target)
//!                 .await
//!                 .map(|_| ())
//!                 .map_err(|_| TransportError::Io(IoErrorKind::Other))
//!         })
//!     }
//!     fn recv_from<'a>(
//!         &'a self,
//!         buf: &'a mut [u8],
//!     ) -> Self::RecvFuture<'a> {
//!         Box::pin(async move {
//!             let (n, src) = self
//!                 .inner
//!                 .recv_from(buf)
//!                 .await
//!                 .map_err(|_| TransportError::Io(IoErrorKind::Other))?;
//!             let source = match src {
//!                 std::net::SocketAddr::V4(v4) => v4,
//!                 std::net::SocketAddr::V6(_) => return Err(TransportError::Unsupported),
//!             };
//!             Ok(ReceivedDatagram {
//!                 bytes_received: n,
//!                 source,
//!                 truncated: false,
//!             })
//!         })
//!     }
//!     fn local_addr(&self) -> Result<SocketAddrV4, TransportError> {
//!         match self.inner.local_addr() {
//!             Ok(std::net::SocketAddr::V4(v4)) => Ok(v4),
//!             Ok(_) => Err(TransportError::Unsupported),
//!             Err(_) => Err(TransportError::Io(IoErrorKind::Other)),
//!         }
//!     }
//!     fn join_multicast_v4(
//!         &self,
//!         group: Ipv4Addr,
//!         iface: Ipv4Addr,
//!     ) -> Result<(), TransportError> {
//!         self.inner
//!             .join_multicast_v4(group, iface)
//!             .map_err(|_| TransportError::Io(IoErrorKind::Other))
//!     }
//!     fn leave_multicast_v4(
//!         &self,
//!         group: Ipv4Addr,
//!         iface: Ipv4Addr,
//!     ) -> Result<(), TransportError> {
//!         self.inner
//!             .leave_multicast_v4(group, iface)
//!             .map_err(|_| TransportError::Io(IoErrorKind::Other))
//!     }
//! }
//!
//! struct TokioTimer;
//! impl Timer for TokioTimer {
//!     fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + Send {
//!         tokio::time::sleep(duration)
//!     }
//! }
//! # }
//! ```
//!
//! # Lifecycle
//!
//! Sockets are dropped to close. There is no explicit `shutdown` method —
//! implementations should release kernel / stack resources in `Drop`.
//! Implementations that need graceful shutdown (flushing an outgoing queue,
//! for example) should perform it in `Drop` or expose an inherent method
//! outside this trait.

use core::future::Future;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::time::Duration;

use crate::e2e::{E2ECheckStatus, E2EKey, E2EProfile};
use crate::e2e::Error as E2EError;

/// Portable I/O error kinds surfaced by transport implementations.
///
/// This is a deliberately small vocabulary — anything that does not fit
/// maps to [`IoErrorKind::Other`]. The enum is `#[non_exhaustive]` so new
/// kinds can be added without a breaking change. Kept local to this crate
/// (rather than re-exporting `embedded_io::ErrorKind`) so our public API
/// does not move when `embedded_io` bumps major versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum IoErrorKind {
    /// The operation timed out.
    #[error("operation timed out")]
    TimedOut,
    /// The operation was interrupted and can be retried.
    #[error("operation interrupted")]
    Interrupted,
    /// The caller lacks permission for the operation.
    #[error("permission denied")]
    PermissionDenied,
    /// A remote peer actively refused the connection / destination was
    /// unreachable.
    #[error("connection refused")]
    ConnectionRefused,
    /// The network layer rejected the operation (routing, MTU, etc.).
    #[error("network unreachable")]
    NetworkUnreachable,
    /// Any error that does not fit a more specific variant.
    #[error("i/o error")]
    Other,
}

/// Errors returned by [`TransportSocket`] and [`TransportFactory`]
/// operations.
///
/// `#[non_exhaustive]` so that backend-specific conditions can be added in
/// future releases without a breaking change. Implementations map their
/// native error types into one of these variants; anything that does not
/// fit a specific variant should use [`TransportError::Io`] with an
/// appropriate [`IoErrorKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TransportError {
    /// Bind failed because the address or port is already in use.
    #[error("address in use")]
    AddressInUse,
    /// The operation is not supported by this transport (for example,
    /// multicast on a backend that has none, or an IPv6 address on an
    /// IPv4-only stack).
    #[error("unsupported transport operation")]
    Unsupported,
    /// A generic I/O error, classified by a portable [`IoErrorKind`].
    #[error("transport i/o: {0}")]
    Io(IoErrorKind),
}

/// Socket-level options applied by [`TransportFactory::bind`].
///
/// The fields mirror the BSD / `socket2` options that `simple-someip`
/// needs for its Service Discovery socket layout. A default-constructed
/// [`SocketOptions`] requests a plain unicast socket.
///
/// `#[non_exhaustive]` so additional knobs (TTL, buffer sizes) can be
/// introduced later without breaking downstream construction.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct SocketOptions {
    /// Enable `SO_REUSEADDR` (required for the SD port 30490 on hosts
    /// that run more than one SOME/IP endpoint on the same interface).
    pub reuse_address: bool,
    /// Enable `SO_REUSEPORT` where supported (Linux, BSD). Ignored on
    /// platforms that do not expose it.
    pub reuse_port: bool,
    /// Outbound multicast interface (`IP_MULTICAST_IF`). `None` lets the
    /// backend choose.
    pub multicast_if_v4: Option<Ipv4Addr>,
    /// Loop multicast traffic back to sockets on the same host
    /// (`IP_MULTICAST_LOOP`). Required when running a SOME/IP server and
    /// client on the same machine for testing.
    pub multicast_loop_v4: bool,
}

impl SocketOptions {
    /// A plain unicast socket with no multicast configuration.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            reuse_address: false,
            reuse_port: false,
            multicast_if_v4: None,
            multicast_loop_v4: false,
        }
    }
}

impl Default for SocketOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// The result of a successful [`TransportSocket::recv_from`].
///
/// `truncated` is set if the backend delivered only a prefix of the
/// incoming datagram because it did not fit in the caller's buffer. If
/// callers use a buffer sized to [`crate::UDP_BUFFER_SIZE`], truncation is
/// generally not expected on backends whose delivered datagrams are
/// bounded by that configured application-level cap. Backends that may
/// deliver larger datagrams should surface this explicitly instead of
/// silently dropping the fact that data was discarded.
///
/// Note: the default Tokio backend currently always reports
/// `truncated: false` because `tokio::net::UdpSocket::recv_from` does not
/// expose `MSG_TRUNC` (or equivalent). Reliable truncation detection
/// requires a backend that does — e.g. a `recvmsg`-based backend, or a
/// `no_std` stack like smoltcp / embassy-net that surfaces the original
/// datagram length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceivedDatagram {
    /// Number of bytes written to the caller's buffer.
    pub bytes_received: usize,
    /// Source address of the datagram.
    pub source: SocketAddrV4,
    /// `true` if the incoming datagram was larger than the caller's
    /// buffer and the tail was discarded. See the type-level docs for
    /// the default Tokio backend's caveat.
    pub truncated: bool,
}

/// A bound, configured UDP socket usable for SOME/IP message exchange.
///
/// Implementations are obtained via [`TransportFactory::bind`]. The
/// send/receive methods return associated future types so callers can
/// require `Send` bounds when spawning socket loops on multithreaded
/// executors. The smaller socket-level queries ([`Self::local_addr`],
/// [`Self::join_multicast_v4`], [`Self::leave_multicast_v4`]) are
/// synchronous because they are typically O(1) lookups on a backend's
/// internal handle and do not benefit from yielding to the executor.
///
/// Multicast group membership is joined *after* bind via
/// [`TransportSocket::join_multicast_v4`]; the bind-time
/// [`SocketOptions::multicast_if_v4`] only selects the *outbound*
/// multicast interface.
///
/// # Associated future types (Phase 12)
///
/// The [`SendFuture`](Self::SendFuture) and [`RecvFuture`](Self::RecvFuture)
/// associated types let consumers express `Send` bounds on the futures
/// returned by `send_to` and `recv_from` without requiring nightly-only
/// Return-Type Notation (RTN, RFC 3654). This enables:
///
/// ```ignore
/// fn spawn_loop<T: TransportSocket>(sock: T, spawner: impl Spawner)
/// where
///     T: Send + Sync + 'static,
///     for<'a> T::SendFuture<'a>: Send,
///     for<'a> T::RecvFuture<'a>: Send,
/// {
///     spawner.spawn(async move { /* use sock */ });
/// }
/// ```
///
/// `TokioSocket` implements these with `Send` futures; bare-metal
/// implementations must do the same if they want to be used with
/// multithreaded spawners.
pub trait TransportSocket {
    /// Future returned by [`Self::send_to`].
    type SendFuture<'a>: Future<Output = Result<(), TransportError>>
    where
        Self: 'a;

    /// Future returned by [`Self::recv_from`].
    type RecvFuture<'a>: Future<Output = Result<ReceivedDatagram, TransportError>>
    where
        Self: 'a;

    /// Send `buf` to `target`. UDP is atomic — either the whole datagram
    /// is transmitted or an error is returned; there is no short-write
    /// case, which is why this method returns `()` on success rather than
    /// a byte count.
    ///
    /// Takes `&self` so a single-task socket loop can hold a pending
    /// [`Self::recv_from`] future and still call `send_to` in another
    /// `select!` branch. Backends that need to mutate their socket
    /// handle on send — e.g. direct smoltcp — must provide interior
    /// mutability (typically `RefCell<_>` on single-threaded `no_std`, or
    /// `critical_section::Mutex<RefCell<_>>` on multi-core HAL). The
    /// `tokio::net::UdpSocket` and `embassy_net::udp::UdpSocket` APIs
    /// are already `&self`, so adapters over those backends need no
    /// extra wrapping.
    ///
    /// # Errors
    ///
    /// Returns:
    /// - [`TransportError::Io`] with the appropriate [`IoErrorKind`] for
    ///   transport-level send failures (e.g. the peer is unreachable,
    ///   the interface is down, the datagram exceeds the link MTU, or a
    ///   platform-level send error).
    /// - [`TransportError::Unsupported`] if `target` is not representable
    ///   on a backend that only speaks a subset of IPv4 (rare; most
    ///   backends surface addressing issues as [`TransportError::Io`]).
    fn send_to<'a>(&'a self, buf: &'a [u8], target: SocketAddrV4) -> Self::SendFuture<'a>;

    /// Receive the next datagram into `buf`, returning a
    /// [`ReceivedDatagram`] carrying byte count, source, and a truncation
    /// flag.
    ///
    /// Takes `&self` for the same reason as [`Self::send_to`]: the
    /// pending receive future must not hold an exclusive borrow of the
    /// socket, or the concurrent send branch of a `select!` cannot
    /// compile.
    ///
    /// # Errors
    ///
    /// Returns:
    /// - [`TransportError::Io`] with the appropriate [`IoErrorKind`] for
    ///   transport-level receive failures (e.g. the socket was closed,
    ///   the interface went down, or a platform-level recv error).
    /// - [`TransportError::Unsupported`] if the backend surfaces a
    ///   non-IPv4 source address that cannot be represented as
    ///   [`SocketAddrV4`].
    ///
    /// A datagram whose payload exceeds `buf` is **not** an error; it is
    /// returned with [`ReceivedDatagram::truncated`] set to `true`. The
    /// caller decides whether to treat truncation as fatal.
    fn recv_from<'a>(&'a self, buf: &'a mut [u8]) -> Self::RecvFuture<'a>;

    /// Return the local address this socket is bound to. Useful for
    /// discovering the ephemeral port chosen by `bind(port: 0, ..)`.
    ///
    /// # Errors
    ///
    /// Returns [`TransportError`] if the backend cannot report the address.
    fn local_addr(&self) -> Result<SocketAddrV4, TransportError>;

    /// Join IPv4 multicast group `group` on interface `iface`. Required
    /// before the socket will receive multicast traffic for that group.
    ///
    /// Called once per group per socket; joining twice is allowed and a
    /// no-op on most backends.
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::Unsupported`] if the backend has no
    /// multicast support; otherwise [`TransportError::Io`] with an
    /// appropriate kind.
    fn join_multicast_v4(&self, group: Ipv4Addr, iface: Ipv4Addr) -> Result<(), TransportError>;

    /// Leave IPv4 multicast group `group` on interface `iface`. Symmetric
    /// to [`Self::join_multicast_v4`]. Most backends implicitly leave on
    /// drop, so this is optional for simple lifetimes but required for
    /// long-lived sockets that rotate group membership.
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::Unsupported`] if the backend has no
    /// multicast support; otherwise [`TransportError::Io`] with an
    /// appropriate kind.
    fn leave_multicast_v4(&self, group: Ipv4Addr, iface: Ipv4Addr) -> Result<(), TransportError>;

    /// Upper bound, in bytes, on datagrams this socket will successfully
    /// accept in `send_to` or return via `recv_from`. The default returns
    /// [`crate::UDP_BUFFER_SIZE`], the crate's default application-level
    /// UDP payload cap (currently 1500 bytes — note that this is *not*
    /// MTU-safe; see [`crate::UDP_BUFFER_SIZE`]'s own docs for the
    /// IPv4/IPv6 header overhead).
    ///
    /// Backends with a smaller effective MTU (for example, some
    /// resource-constrained embedded stacks) should override this to
    /// advertise the real limit so callers can size buffers accordingly.
    #[must_use]
    fn max_datagram_size(&self) -> usize {
        crate::UDP_BUFFER_SIZE
    }
}

/// Constructs [`TransportSocket`] instances from a bind address and
/// [`SocketOptions`]. The factory carries whatever state the backend needs
/// (for example, an lwIP network-interface handle) so that `bind` itself
/// is a pure data operation.
///
/// On `std + tokio`, a unit-struct `TokioTransport;` factory is all that's
/// needed — the runtime is implicit.
pub trait TransportFactory {
    /// The socket type produced by this factory.
    type Socket: TransportSocket;

    /// Bind a new socket to `addr` with the requested `options`.
    ///
    /// `addr.port() == 0` requests an ephemeral port; call
    /// [`TransportSocket::local_addr`] afterwards to discover what was
    /// assigned.
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::AddressInUse`] if the requested address
    /// and port pair is already bound (and `reuse_*` was not enabled).
    /// Other backend-level failures surface as [`TransportError::Io`].
    /// The returned future is required to be `Send` so callers spawning
    /// the bind on a multithreaded executor (e.g. `tokio::spawn` of a
    /// run-loop that internally awaits `bind`) compile cleanly. All
    /// in-tree impls (`TokioTransport`, the bare-metal `MockFactory`,
    /// the embassy adapter) satisfy this; an impl that holds `!Send`
    /// state across a yield in `bind` would need to either lift that
    /// state out or use a `LocalSet`-based spawner.
    fn bind(
        &self,
        addr: SocketAddrV4,
        options: &SocketOptions,
    ) -> impl Future<Output = Result<Self::Socket, TransportError>> + Send;
}

/// Executor-agnostic sleep primitive.
///
/// `simple-someip` needs timed waits in two places: the Service Discovery
/// announcement tick (1 s) and the client event-loop idle timeout
/// (125 ms). Consumers provide a `Timer` at startup; on `std + tokio` this
/// is a one-line wrapper around `tokio::time::sleep`, on embedded it is a
/// one-line wrapper around `embassy_time::Timer::after` or similar.
pub trait Timer {
    /// Wait for at least `duration` before resolving. Implementations MAY
    /// overshoot but MUST NOT undershoot.
    ///
    /// The returned future is required to be `Send` so callers spawning
    /// the sleep on a multithreaded executor (e.g. a `tokio::spawn`-driven
    /// run-loop) compile cleanly. Single-task bare-metal callers whose
    /// `Timer` impl holds `!Send` state across the yield can wrap their
    /// future in a `Send`-compatible adapter or use a `LocalSet`-based
    /// spawner.
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + Send;
}

/// Executor-agnostic task-spawning primitive.
///
/// `simple-someip`'s per-socket I/O loops need to run concurrently with
/// the client's main event loop — otherwise `SocketManager::send`'s
/// internal oneshot wait deadlocks (the send future parks the main
/// loop, which is the only thing that would drive the socket loop to
/// produce its response). Phase 8 hit this and deferred the spawn to
/// a user-provided `Spawner` here, letting std+tokio callers pass a
/// one-line `TokioSpawner` and bare-metal callers wrap their own
/// executor's task-spawning primitive.
///
/// # Why this reverses the phase-4 "no executor adapter" rule
///
/// Phase 4 deliberately avoided wrapping spawn to prevent "reinventing
/// embassy" and trait-object dispatch in the hot path. Concrete
/// evidence from phase 8 showed that without a spawn abstraction,
/// `Inner::bind_*` has to call `tokio::spawn` directly — making the
/// whole crate tokio-only. The revised rule: spawn DOES need a trait,
/// but we avoid the phase-4 concerns by (1) keeping the trait generic
/// (monomorphized, no `dyn Spawner`) and (2) scoping it narrowly —
/// just spawn, not select/sleep which have other solutions.
///
/// # Usage
///
/// On `std + tokio`, use `crate::tokio_transport::TokioSpawner`
/// (available when the `client` or `server` feature is enabled) —
/// a zero-size unit struct whose `spawn` is a thin wrapper around
/// `tokio::spawn`. The path is rendered as a code literal rather
/// than an intra-doc link because the target module is feature-gated
/// and would break default-feature rustdoc builds. On embedded:
///
/// ```ignore
/// struct EmbassySpawner(embassy_executor::Spawner);
/// impl simple_someip::Spawner for EmbassySpawner {
///     fn spawn(&self, fut: impl core::future::Future<Output = ()> + Send + 'static) {
///         // embassy's Spawner has its own task-registration model;
///         // the adapter layer depends on how the user defined their tasks
///         todo!("call self.0.spawn(...)");
///     }
/// }
/// ```
pub trait Spawner {
    /// Submit `future` to the executor. Must not block; must arrange
    /// for the future to be polled to completion on some task.
    ///
    /// # Correctness requirement
    ///
    /// Implementations MUST poll the submitted future. Dropping it
    /// without polling — or holding it in a queue that never drains —
    /// will deadlock `crate::client::Client` (available when the
    /// `client` feature is enabled): `SocketManager::send`
    /// `await`s an internal mpsc→oneshot round-trip whose only driver
    /// is the per-socket loop future submitted here. No poll, no
    /// progress, no oneshot resolution; the caller's `send` hangs
    /// forever.
    ///
    /// The `MockSpawner` in `examples/bare_metal/` deliberately
    /// demonstrates the wrong pattern (drops the future) and annotates
    /// it as DEMO-ONLY for exactly this reason.
    ///
    /// # Fire-and-forget by design
    ///
    /// `spawn` returns `()`, not a join-handle. The rest of the crate
    /// observes `tokio::JoinHandle`s wherever it spawns work directly
    /// (commit `d92c5a3`); this trait is the deliberate exception. The
    /// per-socket loops have no observable result — they run forever and
    /// only exit when their owning `SocketManager` drops its channel
    /// ends — so a join-handle would just be storage with no callers.
    /// A future revision MAY add an associated `Handle` type if a
    /// concrete shutdown / cancellation use case appears; today there is
    /// none.
    ///
    /// # Bound rationale
    ///
    /// The `Send + 'static` bound matches multi-threaded executors like
    /// tokio, async-std, and smol — the captured per-socket loop is
    /// already `Send + 'static` because its underlying `TokioSocket` is.
    /// Embassy and other `no_alloc` / single-core executors typically need
    /// additional adapter scaffolding (a typed `SpawnToken`, a static
    /// task arena, hardware-specific waker plumbing) to satisfy
    /// `Send + 'static`; the example at the top of this docstring has a
    /// `todo!()` precisely because the adapter is not one-line. A future
    /// release MAY add a `spawn_local`-style variant gated on a cargo
    /// feature for those targets.
    fn spawn(&self, future: impl Future<Output = ()> + Send + 'static);
}

/// Shared handle to the runtime E2E configuration registry.
///
/// Abstracts over `Arc<Mutex<E2ERegistry>>` on `std` and over
/// critical-section-backed primitives (e.g. `embassy_sync::blocking_mutex`)
/// on bare metal. All methods take `&self` and provide interior-mutable
/// access. Implementations are required to be `Clone` so the handle can be
/// cheaply shared between the `Client` (or `Server`) handle and its inner
/// event loop.
pub trait E2ERegistryHandle: Clone + Send + Sync + 'static {
    /// Register an E2E profile for the given key, replacing any prior entry.
    fn register(&self, key: E2EKey, profile: E2EProfile);

    /// Remove the E2E configuration for the given key. No-op if absent.
    fn unregister(&self, key: &E2EKey);

    /// Returns `true` if a profile is registered for `key`.
    fn contains_key(&self, key: &E2EKey) -> bool;

    /// Run E2E protect for `key` if configured, writing to `output`.
    ///
    /// Returns `None` if no profile is registered for `key`.
    /// Returns `Some(Err(_))` if protection fails (e.g. buffer too small).
    /// Returns `Some(Ok(len))` on success; `len` is the number of bytes
    /// written to `output`.
    fn protect(
        &self,
        key: E2EKey,
        payload: &[u8],
        upper_header: [u8; 8],
        output: &mut [u8],
    ) -> Option<Result<usize, E2EError>>;

    /// Run E2E check for `key` if configured.
    ///
    /// Returns `None` if no profile is registered for `key`. Otherwise
    /// returns the check status and the effective payload slice — the
    /// E2E header is stripped on success; the original bytes are returned
    /// on check failure so the caller can decide how to handle it.
    ///
    /// The returned slice borrows from `payload`, not from this handle.
    fn check<'a>(
        &self,
        key: E2EKey,
        payload: &'a [u8],
        upper_header: [u8; 8],
    ) -> Option<(E2ECheckStatus, &'a [u8])>;
}

/// Shared handle to the local interface address.
///
/// Abstracts over `Arc<RwLock<Ipv4Addr>>` on `std`. All clones of a
/// `Client` share the same handle, so writes from one clone (e.g.
/// `Client::set_interface`) are visible to all others.
///
/// On bare metal, where `Client` is not `Clone`, a trivial implementation
/// wrapping a `core::cell::Cell<Ipv4Addr>` suffices.
pub trait InterfaceHandle: Clone + Send + Sync + 'static {
    /// Returns the current interface address.
    fn get(&self) -> Ipv4Addr;

    /// Updates the stored interface address.
    fn set(&self, addr: Ipv4Addr);
}

/// Default `std`-flavoured impls of [`E2ERegistryHandle`] and
/// [`InterfaceHandle`] backed by `std::sync::{Arc, Mutex, RwLock}`. Pure
/// std — no tokio dependency — so they live in the executor-agnostic
/// transport module rather than the tokio backend.
#[cfg(feature = "std")]
mod std_handle_impls {
    use super::{E2ERegistryHandle, InterfaceHandle};
    use crate::e2e::{E2ECheckStatus, E2EKey, E2EProfile, E2ERegistry};
    use crate::e2e::Error as E2EError;
    use core::net::Ipv4Addr;
    use std::sync::{Arc, Mutex, RwLock};

    impl E2ERegistryHandle for Arc<Mutex<E2ERegistry>> {
        fn register(&self, key: E2EKey, profile: E2EProfile) {
            self.lock().expect("e2e registry lock poisoned").register(key, profile);
        }

        fn unregister(&self, key: &E2EKey) {
            self.lock().expect("e2e registry lock poisoned").unregister(key);
        }

        fn contains_key(&self, key: &E2EKey) -> bool {
            self.lock().expect("e2e registry lock poisoned").contains_key(key)
        }

        fn protect(
            &self,
            key: E2EKey,
            payload: &[u8],
            upper_header: [u8; 8],
            output: &mut [u8],
        ) -> Option<Result<usize, E2EError>> {
            self.lock()
                .expect("e2e registry lock poisoned")
                .protect(key, payload, upper_header, output)
        }

        fn check<'a>(
            &self,
            key: E2EKey,
            payload: &'a [u8],
            upper_header: [u8; 8],
        ) -> Option<(E2ECheckStatus, &'a [u8])> {
            self.lock()
                .expect("e2e registry lock poisoned")
                .check(key, payload, upper_header)
        }
    }

    impl InterfaceHandle for Arc<RwLock<Ipv4Addr>> {
        fn get(&self) -> Ipv4Addr {
            *self.read().expect("interface lock poisoned")
        }

        fn set(&self, addr: Ipv4Addr) {
            *self.write().expect("interface lock poisoned") = addr;
        }
    }
}

// ── Channel-handle abstraction ────────────────────────────────────────────
//
// `ChannelFactory` and its associated sender / receiver traits abstract over
// the channel primitive used by the client. `TokioChannels` (in
// `tokio_transport`) is the default for `std + tokio` builds;
// `EmbassySyncChannels` (in `crate::embassy_channels`, gated behind
// `bare_metal`) is the alternative for no-tokio / no_std builds.

/// Returned by [`OneshotRecv::recv`] when the sender was dropped before
/// sending a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OneshotCancelled;

impl core::fmt::Display for OneshotCancelled {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("oneshot sender dropped before sending a value")
    }
}

/// The send half of a oneshot channel. Consuming: a value can be sent exactly
/// once.
pub trait OneshotSend<T: Send + 'static>: Send + 'static {
    /// Send `value` through the channel.
    ///
    /// # Errors
    ///
    /// Returns `Err(value)` if the receiver was already dropped.
    fn send(self, value: T) -> Result<(), T>;
}

/// The receive half of a oneshot channel. Resolves once the sender delivers a
/// value, or returns [`OneshotCancelled`] if the sender is dropped first.
pub trait OneshotRecv<T: Send + 'static>: Send + 'static {
    /// Await the value. Consumes self — a oneshot receiver can only be awaited
    /// once.
    fn recv(self) -> impl core::future::Future<Output = Result<T, OneshotCancelled>> + Send;
}

/// The send half of a bounded MPSC channel.
///
/// Implementations must be [`Clone`] so that multiple producers can share the
/// same channel (e.g. the `Client` handle is `Clone` and every clone must be
/// able to send control messages to `Inner`).
pub trait MpscSend<T: Send + 'static>: Clone + Send + 'static {
    /// Send `value`, waiting if the channel is full. Returns `Err(())` if the
    /// receiver was dropped.
    fn send(&self, value: T) -> impl core::future::Future<Output = Result<(), ()>> + Send + '_;
}

/// The receive half of a bounded MPSC channel.
pub trait MpscRecv<T: Send + 'static>: Send + 'static {
    /// Receive the next value, waiting if the channel is empty. Returns `None`
    /// if all senders were dropped and the channel is empty.
    fn recv(&mut self) -> impl core::future::Future<Output = Option<T>> + Send + '_;

    /// Poll the channel without blocking. Used by `receive_any_unicast` to
    /// multiplex across several socket channels in a single `poll_fn` pass.
    fn poll_recv(
        &mut self,
        cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Option<T>>;
}

/// The send half of an unbounded MPSC channel.
///
/// Unlike [`MpscSend`], sending never blocks — the implementation must buffer
/// arbitrarily many values (or, for embassy-sync, use a large finite capacity
/// that is treated as effectively unbounded).
pub trait UnboundedSend<T: Send + 'static>: Clone + Send + 'static {
    /// Send `value` without blocking.
    ///
    /// # Errors
    ///
    /// Returns `Err(value)` if the receiver was dropped.
    fn send_now(&self, value: T) -> Result<(), T>;
}

/// The receive half of an unbounded MPSC channel.
pub trait UnboundedRecv<T: Send + 'static>: Send + 'static {
    /// Receive the next value, waiting if the channel is empty. Returns `None`
    /// if all senders were dropped and the channel is empty.
    fn recv(&mut self) -> impl core::future::Future<Output = Option<T>> + Send + '_;
}

/// A zero-sized factory that creates channel pairs used by the client's
/// internal transport.
///
/// Abstracting over both `tokio::sync::mpsc` / `oneshot` (std path) and
/// `embassy-sync::channel::Channel` (bare-metal path) behind a single trait
/// lets `Client` / `Inner` / `SocketManager` compile without a tokio
/// dependency when `bare_metal` is active and `tokio` is not.
///
/// The three channel families:
/// - **oneshot** — single-shot rendezvous, capacity 1. Used for command
///   completion callbacks inside [`ControlMessage`](crate::client).
/// - **bounded** — finite-capacity MPSC queue. Used for the control channel
///   and per-socket send / receive queues.
/// - **unbounded** — notionally unbounded MPSC queue (embassy-sync
///   implementations use a large-capacity channel). Used for the
///   `ClientUpdate` stream from `Inner` to `Client`.
pub trait ChannelFactory: Clone + Send + Sync + 'static {
    /// Oneshot sender type.
    type OneshotSender<T: Send + 'static>: OneshotSend<T>;
    /// Oneshot receiver type.
    type OneshotReceiver<T: Send + 'static>: OneshotRecv<T>;
    /// Create a oneshot channel pair.
    fn oneshot<T: Send + 'static>() -> (Self::OneshotSender<T>, Self::OneshotReceiver<T>);

    /// Bounded-channel sender type. The `const N: usize` parameter is
    /// the channel capacity; it must match the `N` passed to
    /// [`Self::bounded`]. Backends that store the capacity at
    /// construction time (`tokio::sync::mpsc`) ignore it for storage
    /// purposes; backends that bake it into the type (`embassy-sync`)
    /// use it directly.
    type BoundedSender<T: Send + 'static, const N: usize>: MpscSend<T>;
    /// Bounded-channel receiver type. See [`Self::BoundedSender`].
    type BoundedReceiver<T: Send + 'static, const N: usize>: MpscRecv<T>;
    /// Create a bounded channel with capacity `N`.
    fn bounded<T: Send + 'static, const N: usize>(
    ) -> (Self::BoundedSender<T, N>, Self::BoundedReceiver<T, N>);

    /// Unbounded-channel sender type.
    type UnboundedSender<T: Send + 'static>: UnboundedSend<T>;
    /// Unbounded-channel receiver type.
    type UnboundedReceiver<T: Send + 'static>: UnboundedRecv<T>;
    /// Create an unbounded channel.
    fn unbounded<T: Send + 'static>() -> (Self::UnboundedSender<T>, Self::UnboundedReceiver<T>);
}

#[cfg(test)]
mod tests {
    //! The traits are pure interfaces — these tests only verify that
    //! trivial mock implementations compile and that defaults behave as
    //! documented.

    use super::*;

    /// Drive a Future to completion on the test thread, assuming it never
    /// yields (as with [`core::future::ready`] and its sync-in-disguise
    /// peers). Panics if the future returns `Poll::Pending`.
    fn block_on_ready<F: Future>(fut: F) -> F::Output {
        use core::pin::pin;
        use core::task::{Context, Poll, Waker};
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        let mut fut = pin!(fut);
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => v,
            Poll::Pending => panic!("future yielded Pending; use a real executor"),
        }
    }

    #[test]
    fn socket_options_default_is_plain_unicast() {
        let opts = SocketOptions::default();
        assert!(!opts.reuse_address);
        assert!(!opts.reuse_port);
        assert!(opts.multicast_if_v4.is_none());
        assert!(!opts.multicast_loop_v4);
    }

    #[test]
    fn socket_options_new_matches_default() {
        let a = SocketOptions::new();
        let b = SocketOptions::default();
        assert_eq!(a.reuse_address, b.reuse_address);
        assert_eq!(a.reuse_port, b.reuse_port);
        assert_eq!(a.multicast_if_v4, b.multicast_if_v4);
        assert_eq!(a.multicast_loop_v4, b.multicast_loop_v4);
    }

    // A minimal `TransportSocket` + `TransportFactory` + `Timer`
    // implementation. Exists purely to prove the trait signatures are
    // implementable with zero `async` machinery — the futures are produced
    // by `core::future` primitives, no executor involved. If this module
    // compiles, any tokio / embassy / smoltcp adapter will also compile.
    struct NullSocket {
        addr: SocketAddrV4,
    }

    impl TransportSocket for NullSocket {
        type SendFuture<'a> = core::future::Ready<Result<(), TransportError>>;
        type RecvFuture<'a> = core::future::Ready<Result<ReceivedDatagram, TransportError>>;

        fn send_to<'a>(&'a self, _buf: &'a [u8], _target: SocketAddrV4) -> Self::SendFuture<'a> {
            core::future::ready(Err(TransportError::Unsupported))
        }

        fn recv_from<'a>(&'a self, _buf: &'a mut [u8]) -> Self::RecvFuture<'a> {
            core::future::ready(Err(TransportError::Unsupported))
        }

        fn local_addr(&self) -> Result<SocketAddrV4, TransportError> {
            Ok(self.addr)
        }

        fn join_multicast_v4(
            &self,
            _group: Ipv4Addr,
            _iface: Ipv4Addr,
        ) -> Result<(), TransportError> {
            Err(TransportError::Unsupported)
        }

        fn leave_multicast_v4(
            &self,
            _group: Ipv4Addr,
            _iface: Ipv4Addr,
        ) -> Result<(), TransportError> {
            Err(TransportError::Unsupported)
        }
    }

    struct NullFactory;

    impl TransportFactory for NullFactory {
        type Socket = NullSocket;

        fn bind(
            &self,
            addr: SocketAddrV4,
            _options: &SocketOptions,
        ) -> impl Future<Output = Result<Self::Socket, TransportError>> {
            core::future::ready(Ok(NullSocket { addr }))
        }
    }

    struct NullTimer;

    impl Timer for NullTimer {
        fn sleep(&self, _duration: Duration) -> impl Future<Output = ()> {
            core::future::ready(())
        }
    }

    #[test]
    fn null_factory_bind_resolves_with_addr() {
        let factory = NullFactory;
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
        let options = SocketOptions::default();
        let sock = block_on_ready(factory.bind(addr, &options)).expect("bind");
        assert_eq!(sock.local_addr().unwrap(), addr);
    }

    #[test]
    fn max_datagram_size_default_is_udp_buffer_size() {
        let sock = NullSocket {
            addr: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
        };
        assert_eq!(sock.max_datagram_size(), crate::UDP_BUFFER_SIZE);
    }

    #[test]
    fn null_timer_sleep_resolves_immediately() {
        let timer = NullTimer;
        block_on_ready(timer.sleep(Duration::from_secs(1)));
    }

    #[test]
    fn received_datagram_construct_and_field_access() {
        let d = ReceivedDatagram {
            bytes_received: 42,
            source: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9999),
            truncated: false,
        };
        assert_eq!(d.bytes_received, 42);
        assert!(!d.truncated);
    }

    #[test]
    fn io_error_kind_variants_are_distinct() {
        // Compile-time check that all variants are constructible and
        // distinguishable — Eq is derived, so assert some inequalities.
        assert_ne!(IoErrorKind::TimedOut, IoErrorKind::Interrupted);
        assert_ne!(IoErrorKind::PermissionDenied, IoErrorKind::Other);
        assert_ne!(
            IoErrorKind::ConnectionRefused,
            IoErrorKind::NetworkUnreachable
        );
    }

    #[test]
    fn transport_error_io_wraps_kind() {
        let e = TransportError::Io(IoErrorKind::TimedOut);
        assert_eq!(e, TransportError::Io(IoErrorKind::TimedOut));
        assert_ne!(e, TransportError::AddressInUse);
    }

    // Minimal no-op implementations to verify that E2ERegistryHandle and
    // InterfaceHandle are implementable without any executor machinery.
    #[derive(Clone)]
    struct NullE2ERegistry;

    impl E2ERegistryHandle for NullE2ERegistry {
        fn register(&self, _key: E2EKey, _profile: E2EProfile) {}
        fn unregister(&self, _key: &E2EKey) {}
        fn contains_key(&self, _key: &E2EKey) -> bool {
            false
        }
        fn protect(
            &self,
            _key: E2EKey,
            _payload: &[u8],
            _upper_header: [u8; 8],
            _output: &mut [u8],
        ) -> Option<Result<usize, E2EError>> {
            None
        }
        fn check<'a>(
            &self,
            _key: E2EKey,
            _payload: &'a [u8],
            _upper_header: [u8; 8],
        ) -> Option<(E2ECheckStatus, &'a [u8])> {
            None
        }
    }

    #[derive(Clone)]
    struct NullInterface(Ipv4Addr);

    impl InterfaceHandle for NullInterface {
        fn get(&self) -> Ipv4Addr {
            self.0
        }
        fn set(&self, _addr: Ipv4Addr) {}
    }

    #[test]
    fn null_e2e_registry_compiles() {
        let r = NullE2ERegistry;
        let key = E2EKey::new(0, 0);
        r.register(key, crate::e2e::E2EProfile::Profile4(
            crate::e2e::Profile4Config::new(0, 8),
        ));
        assert!(!r.contains_key(&key));
        assert!(r.check(key, b"hello", [0; 8]).is_none());
    }

    #[test]
    fn null_interface_get_set() {
        let h = NullInterface(Ipv4Addr::LOCALHOST);
        assert_eq!(h.get(), Ipv4Addr::LOCALHOST);
        h.set(Ipv4Addr::UNSPECIFIED); // no-op in null impl
        assert_eq!(h.get(), Ipv4Addr::LOCALHOST); // unchanged
    }
}
