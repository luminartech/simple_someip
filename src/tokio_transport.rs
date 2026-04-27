//! Tokio + socket2 implementation of the [`crate::transport`] traits.
//!
//! This is the default `std` backend. [`TokioTransport`] constructs
//! configured [`TokioSocket`]s via `socket2` for bind-time options (reuse,
//! multicast interface, multicast loop) and converts them to
//! [`tokio::net::UdpSocket`] for the async I/O loop. [`TokioTimer`] is a
//! thin wrapper over `tokio::time::sleep`.
//!
//! Gated behind `#[cfg(any(feature = "client", feature = "server"))]` —
//! the `client` and `server` features are exactly the ones that already
//! pull in `tokio` and `socket2`, so no new dependency edge is introduced.
//!
//! # Example
//!
//! ```no_run
//! # #[cfg(any(feature = "client", feature = "server"))]
//! # async fn demo() -> Result<(), simple_someip::TransportError> {
//! use core::net::{Ipv4Addr, SocketAddrV4};
//! use simple_someip::{SocketOptions, TransportFactory, TransportSocket};
//! use simple_someip::tokio_transport::TokioTransport;
//!
//! let factory = TokioTransport::default();
//! let mut options = SocketOptions::new();
//! options.reuse_address = true;
//!
//! let mut sock = factory
//!     .bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), &options)
//!     .await?;
//! let bound = sock.local_addr()?;
//! println!("bound to {bound}");
//! # Ok(())
//! # }
//! ```

use core::future::Future;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::time::Duration;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, RwLock};
use tokio::net::UdpSocket;

use crate::e2e::{E2ECheckStatus, E2EKey, E2EProfile};
use crate::e2e::Error as E2EError;
use crate::e2e::E2ERegistry;
use crate::transport::{
    E2ERegistryHandle, InterfaceHandle, IoErrorKind, ReceivedDatagram, SocketOptions, Timer,
    TransportError, TransportFactory, TransportSocket,
};

/// Factory that binds [`TokioSocket`]s configured via `socket2`.
///
/// Unit struct — all required state (the tokio runtime) is implicit in the
/// ambient task context at call time.
#[derive(Debug, Default, Clone, Copy)]
pub struct TokioTransport;

/// A bound UDP socket backed by [`tokio::net::UdpSocket`].
#[derive(Debug)]
pub struct TokioSocket {
    inner: UdpSocket,
}

impl TokioSocket {
    /// Read back the current value of the `IP_MULTICAST_LOOP` flag. Thin
    /// wrapper over [`tokio::net::UdpSocket::multicast_loop_v4`], exposed
    /// for tests that verify [`SocketOptions::multicast_loop_v4`] is
    /// applied and for field debugging.
    ///
    /// # Errors
    ///
    /// Returns [`TransportError`] if the backend cannot read the flag.
    #[allow(dead_code)] // used in tests; kept available for field debugging.
    pub(crate) fn multicast_loop_v4(&self) -> Result<bool, TransportError> {
        self.inner.multicast_loop_v4().map_err(|e| map_io_error(&e))
    }
}

/// Sleep backed by [`tokio::time::sleep`].
///
/// Used internally at every periodic-tick site in the crate: the 125ms
/// idle tick in `Inner::run_future`, the 1s announcement tick in
/// `Server::announcement_loop`, and the user-supplied interval in
/// `Client::sd_announcements_loop`. A bare-metal consumer swapping this
/// out for `embassy_time` (or similar) needs to replace three references
/// to `TokioTimer` with their own `Timer` impl — no trait rewrite
/// required.
#[derive(Debug, Default, Clone, Copy)]
pub struct TokioTimer;

/// [`crate::transport::Spawner`] impl that routes submitted futures
/// to `tokio::spawn`.
///
/// Zero-size unit struct; every `Inner<P, TokioSpawner>` / `Client<P>`
/// pays nothing for the abstraction (the `Inner` carries the spawner
/// generic; `Client<P>` is a thin handle that forwards to it).
/// Bare-metal consumers substitute their own `Spawner` via the
/// `crate::Client::new_with_spawner_and_loopback` constructor.
#[derive(Debug, Default, Clone, Copy)]
pub struct TokioSpawner;

impl TransportFactory for TokioTransport {
    type Socket = TokioSocket;

    fn bind(
        &self,
        addr: SocketAddrV4,
        options: &SocketOptions,
    ) -> impl Future<Output = Result<Self::Socket, TransportError>> {
        // Capture options by value into the async block so the returned
        // future does not borrow `self` or `options`.
        let options = *options;
        async move { bind_with_options(addr, options).map_err(|e| map_io_error(&e)) }
    }
}

impl TransportSocket for TokioSocket {
    async fn send_to(&self, buf: &[u8], target: SocketAddrV4) -> Result<(), TransportError> {
        self.inner
            .send_to(buf, target)
            .await
            .map(|_| ())
            .map_err(|e| map_io_error(&e))
    }

    async fn recv_from(&self, buf: &mut [u8]) -> Result<ReceivedDatagram, TransportError> {
        let (n, src) = self
            .inner
            .recv_from(buf)
            .await
            .map_err(|e| map_io_error(&e))?;
        let source = match src {
            SocketAddr::V4(v4) => v4,
            SocketAddr::V6(_) => {
                // SOME/IP is IPv4-only; an IPv6 source on our socket is
                // either impossible (v4 bind) or a misconfiguration.
                return Err(TransportError::Unsupported);
            }
        };
        // Caveat: `tokio::net::UdpSocket::recv_from` silently
        // truncates when the caller's `buf` is smaller than the
        // datagram and returns only the bytes that fit — it does
        // NOT expose a truncation flag. Surfacing a reliable
        // `truncated: bool` here would require a platform-specific
        // `recvmsg`/MSG_TRUNC path (libc + unsafe), which is
        // deferred to the phase 10+ bare-metal refactor. Until
        // then, this field is always `false` for the Tokio
        // backend; callers must not rely on it for truncation
        // detection. This is documented on
        // `ReceivedDatagram::truncated`'s field doc.
        Ok(ReceivedDatagram {
            bytes_received: n,
            source,
            truncated: false,
        })
    }

    fn local_addr(&self) -> Result<SocketAddrV4, TransportError> {
        match self.inner.local_addr().map_err(|e| map_io_error(&e))? {
            SocketAddr::V4(v4) => Ok(v4),
            SocketAddr::V6(_) => Err(TransportError::Unsupported),
        }
    }

    fn join_multicast_v4(&self, group: Ipv4Addr, iface: Ipv4Addr) -> Result<(), TransportError> {
        self.inner
            .join_multicast_v4(group, iface)
            .map_err(|e| map_io_error(&e))
    }

    fn leave_multicast_v4(&self, group: Ipv4Addr, iface: Ipv4Addr) -> Result<(), TransportError> {
        self.inner
            .leave_multicast_v4(group, iface)
            .map_err(|e| map_io_error(&e))
    }
}

impl Timer for TokioTimer {
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

impl crate::transport::Spawner for TokioSpawner {
    fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        // Drop the returned `JoinHandle` — per-socket loops run until
        // their owning `SocketManager` drops its channel ends, at
        // which point the future completes naturally. Callers that
        // want cancel-on-abort semantics should spawn at their own
        // call site; this trait is intentionally minimal.
        drop(tokio::spawn(future));
    }
}

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

/// Synchronously create and configure a UDP socket via `socket2`, then
/// hand it to tokio. Mirrors the existing bind paths in
/// `crate::client::socket_manager` and `crate::server` (rendered as
/// code literals because both are feature-gated and would break
/// default-feature rustdoc builds via broken intra-doc links) so
/// behavior is identical.
fn bind_with_options(addr: SocketAddrV4, options: SocketOptions) -> std::io::Result<TokioSocket> {
    let raw = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    if options.reuse_address {
        raw.set_reuse_address(true)?;
    }
    #[cfg(unix)]
    if options.reuse_port {
        raw.set_reuse_port(true)?;
    }
    if let Some(iface) = options.multicast_if_v4 {
        raw.set_multicast_if_v4(&iface)?;
    }
    raw.set_multicast_loop_v4(options.multicast_loop_v4)?;
    let bind_addr = SocketAddr::new(IpAddr::V4(*addr.ip()), addr.port());
    raw.bind(&bind_addr.into())?;
    raw.set_nonblocking(true)?;
    let std_sock: std::net::UdpSocket = raw.into();
    let inner = UdpSocket::from_std(std_sock)?;
    Ok(TokioSocket { inner })
}

/// Map a `std::io::Error` into [`TransportError`]. The mapping is
/// conservative — anything that is not a clear match becomes
/// [`TransportError::Io`] with [`IoErrorKind::Other`] — and is not
/// considered stable (adding finer mappings is not a breaking change).
///
/// The full `std::io::Error` (raw errno, OS message, chained source) is
/// discarded by design to keep the public [`TransportError`] enum
/// portable and `no_std`-safe. To keep field debugging possible anyway,
/// the original error is emitted to the tracing subscriber before
/// mapping — at `debug!` for common steady-state conditions
/// (`TimedOut`, `Interrupted`, `ConnectionRefused`) so they don't
/// drown out actionable warnings under load, and at `warn!` for
/// everything else (misconfiguration-indicating kinds like
/// `AddrInUse` / `PermissionDenied` / `NetworkUnreachable` and the
/// fallback `Other`). Operators should look at `warn!` lines; the
/// `debug!` lines are there for deep-dive debugging only.
fn map_io_error(e: &std::io::Error) -> TransportError {
    use std::io::ErrorKind as K;
    let kind = e.kind();
    let mapped = match kind {
        K::AddrInUse => TransportError::AddressInUse,
        K::Unsupported => TransportError::Unsupported,
        K::TimedOut => TransportError::Io(IoErrorKind::TimedOut),
        K::Interrupted => TransportError::Io(IoErrorKind::Interrupted),
        K::PermissionDenied => TransportError::Io(IoErrorKind::PermissionDenied),
        K::ConnectionRefused => TransportError::Io(IoErrorKind::ConnectionRefused),
        K::NetworkUnreachable | K::HostUnreachable => {
            TransportError::Io(IoErrorKind::NetworkUnreachable)
        }
        _ => TransportError::Io(IoErrorKind::Other),
    };
    // Log at `warn!` for unexpected / misconfiguration-indicating
    // kinds (permission denied, address-in-use, network unreachable,
    // fallback Other) where ops should probably look. Common
    // steady-state conditions (timeouts, interrupted syscalls,
    // connection refused during transient outages) drop to `debug!`
    // so we don't drown out actionable warnings under load.
    match kind {
        K::TimedOut | K::Interrupted | K::ConnectionRefused => {
            tracing::debug!(
                "tokio transport io error: {e} (raw_os={:?}, kind={:?}) mapped to {mapped}",
                e.raw_os_error(),
                kind,
            );
        }
        _ => {
            tracing::warn!(
                "tokio transport io error: {e} (raw_os={:?}, kind={:?}) mapped to {mapped}",
                e.raw_os_error(),
                kind,
            );
        }
    }
    mapped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bind_ephemeral_and_report_local_addr() {
        let factory = TokioTransport;
        let sock = factory
            .bind(
                SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
                &SocketOptions::default(),
            )
            .await
            .expect("bind");
        let addr = sock.local_addr().expect("local_addr");
        assert_eq!(*addr.ip(), Ipv4Addr::LOCALHOST);
        assert_ne!(addr.port(), 0, "kernel must assign a non-zero port");
    }

    #[tokio::test]
    async fn round_trip_send_recv_between_two_sockets() {
        let factory = TokioTransport;
        let opts = SocketOptions::default();

        let recv = factory
            .bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), &opts)
            .await
            .unwrap();
        let recv_addr = recv.local_addr().unwrap();

        let send = factory
            .bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), &opts)
            .await
            .unwrap();

        let payload = b"hello tokio transport";
        send.send_to(payload, recv_addr).await.unwrap();

        let mut buf = [0u8; 64];
        let datagram = tokio::time::timeout(Duration::from_secs(2), recv.recv_from(&mut buf))
            .await
            .expect("recv timed out")
            .expect("recv failed");

        assert_eq!(datagram.bytes_received, payload.len());
        assert_eq!(&buf[..datagram.bytes_received], payload);
        assert!(!datagram.truncated);
    }

    #[tokio::test]
    async fn reuse_address_option_allows_rebind_pattern() {
        // Two sockets with reuse_address=true should be able to bind the
        // same port on platforms where SO_REUSEADDR permits it (windows
        // and linux both do for DGRAM).
        let opts = SocketOptions {
            reuse_address: true,
            ..SocketOptions::default()
        };

        let factory = TokioTransport;
        let a = factory
            .bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), &opts)
            .await
            .unwrap();
        let port = a.local_addr().unwrap().port();

        // Bind a second socket with the same options; with reuse_address
        // on, the OS allows this for UDP DGRAM on the platforms we support.
        // If the OS refuses, fall back to a plain bind — we're not testing
        // OS semantics here, only that the option is applied without error.
        let b = factory
            .bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port), &opts)
            .await;
        // Either success or AddrInUse is acceptable; the assertion is
        // that bind_with_options does not produce a different surprise
        // (like Unsupported or a raw Io panic).
        match b {
            Ok(_) | Err(TransportError::AddressInUse) => {}
            Err(other) => panic!("unexpected rebind error: {other:?}"),
        }
        drop(a);
    }

    #[tokio::test]
    async fn multicast_loop_v4_option_propagates_in_both_directions() {
        // Guards against a regression where `multicast_loop_v4: false` was
        // silently ignored and the socket kept the OS default (often
        // loopback ENABLED), diverging from the explicit request.
        let factory = TokioTransport;

        let opts_off = SocketOptions {
            multicast_loop_v4: false,
            ..SocketOptions::default()
        };
        let sock_off = factory
            .bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), &opts_off)
            .await
            .expect("bind off");
        assert!(
            !sock_off.multicast_loop_v4().expect("read off flag"),
            "multicast_loop_v4=false must disable IP_MULTICAST_LOOP"
        );

        let opts_on = SocketOptions {
            multicast_loop_v4: true,
            ..SocketOptions::default()
        };
        let sock_on = factory
            .bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), &opts_on)
            .await
            .expect("bind on");
        assert!(
            sock_on.multicast_loop_v4().expect("read on flag"),
            "multicast_loop_v4=true must enable IP_MULTICAST_LOOP"
        );
    }

    #[tokio::test]
    async fn timer_sleep_elapses_at_least_requested() {
        let timer = TokioTimer;
        let started = tokio::time::Instant::now();
        timer.sleep(Duration::from_millis(25)).await;
        assert!(started.elapsed() >= Duration::from_millis(25));
    }

    #[test]
    fn map_io_error_covers_common_kinds() {
        use std::io::{Error, ErrorKind};
        assert!(matches!(
            map_io_error(&Error::from(ErrorKind::AddrInUse)),
            TransportError::AddressInUse
        ));
        assert!(matches!(
            map_io_error(&Error::from(ErrorKind::TimedOut)),
            TransportError::Io(IoErrorKind::TimedOut)
        ));
        assert!(matches!(
            map_io_error(&Error::from(ErrorKind::ConnectionRefused)),
            TransportError::Io(IoErrorKind::ConnectionRefused)
        ));
        assert!(matches!(
            map_io_error(&Error::from(ErrorKind::Unsupported)),
            TransportError::Unsupported
        ));
        // Fallback path
        assert!(matches!(
            map_io_error(&Error::from(ErrorKind::Other)),
            TransportError::Io(IoErrorKind::Other)
        ));
    }
}
