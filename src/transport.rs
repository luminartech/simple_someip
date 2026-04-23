//! Executor-agnostic transport abstraction.
//!
//! [`TransportSocket`] is the minimum UDP surface `simple-someip` needs from
//! its networking backend: unicast and multicast send/recv plus a few
//! socket-level knobs. [`TransportFactory`] constructs bound and configured
//! sockets at startup. [`Timer`] provides async sleep.
//!
//! # Why a trait, and why like this
//!
//! The crate's `client` and `server` modules today bind `tokio::net::UdpSocket`
//! directly. That works on `std + tokio` but makes no-`std` / non-tokio
//! embedded use impossible. These traits are the integration point for
//! alternative backends (lwIP, smoltcp, etc.).
//!
//! Three explicit design choices:
//!
//! 1. **Executor-agnostic.** Methods return `impl Future`, not `async fn`,
//!    and the traits make no statement about `Send` or `'static` bounds on
//!    the returned futures. Callers that need those bounds (e.g. to
//!    `tokio::spawn`) require them at the consumer site. Bare-metal callers
//!    driving the future on a single executor task pay no `Send` tax.
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
//! The traits are defined but not yet wired into `Client`/`Server`; that is
//! the next refactor step. No implementations ship with the crate yet.
//! Callers must provide their own backend — typically a thin adapter over
//! `tokio::net::UdpSocket` + `tokio::time` on `std`, or over
//! `smoltcp::UdpSocket` + `embassy-time` on embedded.
//!
//! # Minimal adapter sketch
//!
//! ```
//! # #[cfg(feature = "client")]
//! # fn wrapper() {
//! use core::future::Future;
//! use core::net::{Ipv4Addr, SocketAddrV4};
//! use core::time::Duration;
//! use simple_someip::transport::{
//!     IoErrorKind, ReceivedDatagram, SocketOptions, Timer, TransportError,
//!     TransportFactory, TransportSocket,
//! };
//!
//! pub struct TokioTransport;
//!
//! pub struct TokioSocket {
//!     inner: tokio::net::UdpSocket,
//! }
//!
//! impl TransportFactory for TokioTransport {
//!     type Socket = TokioSocket;
//!     fn bind(
//!         &self,
//!         addr: SocketAddrV4,
//!         _options: &SocketOptions,
//!     ) -> impl Future<Output = Result<Self::Socket, TransportError>> {
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
//!     fn send_to(
//!         &mut self,
//!         buf: &[u8],
//!         target: SocketAddrV4,
//!     ) -> impl Future<Output = Result<(), TransportError>> {
//!         async move {
//!             self.inner
//!                 .send_to(buf, target)
//!                 .await
//!                 .map(|_| ())
//!                 .map_err(|_| TransportError::Io(IoErrorKind::Other))
//!         }
//!     }
//!     fn recv_from(
//!         &mut self,
//!         buf: &mut [u8],
//!     ) -> impl Future<Output = Result<ReceivedDatagram, TransportError>> {
//!         async move {
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
//!         }
//!     }
//!     fn local_addr(&self) -> Result<SocketAddrV4, TransportError> {
//!         match self.inner.local_addr() {
//!             Ok(std::net::SocketAddr::V4(v4)) => Ok(v4),
//!             Ok(_) => Err(TransportError::Unsupported),
//!             Err(_) => Err(TransportError::Io(IoErrorKind::Other)),
//!         }
//!     }
//!     fn join_multicast_v4(
//!         &mut self,
//!         group: Ipv4Addr,
//!         iface: Ipv4Addr,
//!     ) -> Result<(), TransportError> {
//!         self.inner
//!             .join_multicast_v4(group, iface)
//!             .map_err(|_| TransportError::Io(IoErrorKind::Other))
//!     }
//!     fn leave_multicast_v4(
//!         &mut self,
//!         group: Ipv4Addr,
//!         iface: Ipv4Addr,
//!     ) -> Result<(), TransportError> {
//!         self.inner
//!             .leave_multicast_v4(group, iface)
//!             .map_err(|_| TransportError::Io(IoErrorKind::Other))
//!     }
//! }
//!
//! pub struct TokioTimer;
//! impl Timer for TokioTimer {
//!     fn sleep(&self, duration: Duration) -> impl Future<Output = ()> {
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

/// Portable I/O error kinds surfaced by transport implementations.
///
/// This is a deliberately small vocabulary — anything that does not fit
/// maps to [`IoErrorKind::Other`]. The enum is `#[non_exhaustive]` so new
/// kinds can be added without a breaking change. Kept local to this crate
/// (rather than re-exporting `embedded_io::ErrorKind`) so our public API
/// does not move when `embedded_io` bumps major versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum IoErrorKind {
    /// The operation timed out.
    TimedOut,
    /// The operation was interrupted and can be retried.
    Interrupted,
    /// The caller lacks permission for the operation.
    PermissionDenied,
    /// A remote peer actively refused the connection / destination was
    /// unreachable.
    ConnectionRefused,
    /// The network layer rejected the operation (routing, MTU, etc.).
    NetworkUnreachable,
    /// Any error that does not fit a more specific variant.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransportError {
    /// Bind failed because the address or port is already in use.
    AddressInUse,
    /// The operation is not supported by this transport (for example,
    /// multicast on a backend that has none, or an IPv6 address on an
    /// IPv4-only stack).
    Unsupported,
    /// A generic I/O error, classified by a portable [`IoErrorKind`].
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
/// incoming datagram because it did not fit in the caller's buffer.
/// On backends that size `buf` at least as large as the link MTU (the
/// expected configuration — see [`crate::UDP_BUFFER_SIZE`]), truncation
/// should not occur in practice; the field exists so backends that cannot
/// guarantee this can surface it explicitly instead of silently dropping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceivedDatagram {
    /// Number of bytes written to the caller's buffer.
    pub bytes_received: usize,
    /// Source address of the datagram.
    pub source: SocketAddrV4,
    /// `true` if the incoming datagram was larger than the caller's
    /// buffer and the tail was discarded.
    pub truncated: bool,
}

/// A bound, configured UDP socket usable for SOME/IP message exchange.
///
/// Implementations are obtained via [`TransportFactory::bind`]. All I/O
/// methods return `impl Future` so the trait is executor-agnostic; the
/// caller awaits them on whatever runtime it owns.
///
/// Multicast group membership is joined *after* bind via
/// [`TransportSocket::join_multicast_v4`]; the bind-time
/// [`SocketOptions::multicast_if_v4`] only selects the *outbound*
/// multicast interface.
pub trait TransportSocket {
    /// Send `buf` to `target`. UDP is atomic — either the whole datagram
    /// is transmitted or an error is returned; there is no short-write
    /// case, which is why this method returns `()` on success rather than
    /// a byte count.
    fn send_to(
        &mut self,
        buf: &[u8],
        target: SocketAddrV4,
    ) -> impl Future<Output = Result<(), TransportError>>;

    /// Receive the next datagram into `buf`, returning a
    /// [`ReceivedDatagram`] carrying byte count, source, and a truncation
    /// flag.
    fn recv_from(
        &mut self,
        buf: &mut [u8],
    ) -> impl Future<Output = Result<ReceivedDatagram, TransportError>>;

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
    fn join_multicast_v4(&mut self, group: Ipv4Addr, iface: Ipv4Addr)
    -> Result<(), TransportError>;

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
    fn leave_multicast_v4(
        &mut self,
        group: Ipv4Addr,
        iface: Ipv4Addr,
    ) -> Result<(), TransportError>;

    /// Upper bound, in bytes, on datagrams this socket will successfully
    /// accept in `send_to` or return via `recv_from`. The default returns
    /// [`crate::UDP_BUFFER_SIZE`] (1500), matching standard Ethernet MTU.
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
    fn bind(
        &self,
        addr: SocketAddrV4,
        options: &SocketOptions,
    ) -> impl Future<Output = Result<Self::Socket, TransportError>>;
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
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()>;
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
        fn send_to(
            &mut self,
            _buf: &[u8],
            _target: SocketAddrV4,
        ) -> impl Future<Output = Result<(), TransportError>> {
            core::future::ready(Err(TransportError::Unsupported))
        }

        fn recv_from(
            &mut self,
            _buf: &mut [u8],
        ) -> impl Future<Output = Result<ReceivedDatagram, TransportError>> {
            core::future::ready(Err(TransportError::Unsupported))
        }

        fn local_addr(&self) -> Result<SocketAddrV4, TransportError> {
            Ok(self.addr)
        }

        fn join_multicast_v4(
            &mut self,
            _group: Ipv4Addr,
            _iface: Ipv4Addr,
        ) -> Result<(), TransportError> {
            Err(TransportError::Unsupported)
        }

        fn leave_multicast_v4(
            &mut self,
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
}
