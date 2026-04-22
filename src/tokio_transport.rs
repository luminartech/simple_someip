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
use tokio::net::UdpSocket;

use crate::transport::{
    IoErrorKind, ReceivedDatagram, SocketOptions, Timer, TransportError, TransportFactory,
    TransportSocket,
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

/// Sleep backed by [`tokio::time::sleep`].
#[derive(Debug, Default, Clone, Copy)]
pub struct TokioTimer;

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
        async move { bind_with_options(addr, &options).map_err(map_io_error) }
    }
}

impl TransportSocket for TokioSocket {
    fn send_to(
        &mut self,
        buf: &[u8],
        target: SocketAddrV4,
    ) -> impl Future<Output = Result<(), TransportError>> {
        async move {
            self.inner
                .send_to(buf, target)
                .await
                .map(|_| ())
                .map_err(map_io_error)
        }
    }

    fn recv_from(
        &mut self,
        buf: &mut [u8],
    ) -> impl Future<Output = Result<ReceivedDatagram, TransportError>> {
        async move {
            let (n, src) = self.inner.recv_from(buf).await.map_err(map_io_error)?;
            let source = match src {
                SocketAddr::V4(v4) => v4,
                SocketAddr::V6(_) => {
                    // SOME/IP is IPv4-only; an IPv6 source on our socket is
                    // either impossible (v4 bind) or a misconfiguration.
                    return Err(TransportError::Unsupported);
                }
            };
            Ok(ReceivedDatagram {
                bytes_received: n,
                source,
                truncated: false,
            })
        }
    }

    fn local_addr(&self) -> Result<SocketAddrV4, TransportError> {
        match self.inner.local_addr().map_err(map_io_error)? {
            SocketAddr::V4(v4) => Ok(v4),
            SocketAddr::V6(_) => Err(TransportError::Unsupported),
        }
    }

    fn join_multicast_v4(
        &mut self,
        group: Ipv4Addr,
        iface: Ipv4Addr,
    ) -> Result<(), TransportError> {
        self.inner
            .join_multicast_v4(group, iface)
            .map_err(map_io_error)
    }

    fn leave_multicast_v4(
        &mut self,
        group: Ipv4Addr,
        iface: Ipv4Addr,
    ) -> Result<(), TransportError> {
        self.inner
            .leave_multicast_v4(group, iface)
            .map_err(map_io_error)
    }
}

impl Timer for TokioTimer {
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> {
        // tokio::time::sleep returns a Sleep future; we wrap in an async
        // block so the returned type is a simple `impl Future<Output = ()>`.
        async move { tokio::time::sleep(duration).await }
    }
}

/// Synchronously create and configure a UDP socket via `socket2`, then
/// hand it to tokio. Mirrors the existing bind paths in
/// [`crate::client::socket_manager`] and [`crate::server`] so behavior is
/// identical.
fn bind_with_options(addr: SocketAddrV4, options: &SocketOptions) -> std::io::Result<TokioSocket> {
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
    if options.multicast_loop_v4 {
        raw.set_multicast_loop_v4(true)?;
    }
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
fn map_io_error(e: std::io::Error) -> TransportError {
    use std::io::ErrorKind as K;
    match e.kind() {
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
    }
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

        let mut recv = factory
            .bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), &opts)
            .await
            .unwrap();
        let recv_addr = recv.local_addr().unwrap();

        let mut send = factory
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
        let mut opts = SocketOptions::default();
        opts.reuse_address = true;

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
            map_io_error(Error::from(ErrorKind::AddrInUse)),
            TransportError::AddressInUse
        ));
        assert!(matches!(
            map_io_error(Error::from(ErrorKind::TimedOut)),
            TransportError::Io(IoErrorKind::TimedOut)
        ));
        assert!(matches!(
            map_io_error(Error::from(ErrorKind::ConnectionRefused)),
            TransportError::Io(IoErrorKind::ConnectionRefused)
        ));
        assert!(matches!(
            map_io_error(Error::from(ErrorKind::Unsupported)),
            TransportError::Unsupported
        ));
        // Fallback path
        assert!(matches!(
            map_io_error(Error::from(ErrorKind::Other)),
            TransportError::Io(IoErrorKind::Other)
        ));
    }
}
