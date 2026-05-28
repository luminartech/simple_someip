//! Tokio-backed implementations of [`AsyncUdpSocket`] and [`Clock`].
//!
//! Wraps `tokio::net::UdpSocket` and `tokio::time` so the generic SOME/IP
//! client and server (introduced in the runtime-agnostic refactor) can run
//! unchanged on desktop targets.

use core::net::{Ipv4Addr, SocketAddrV4};
use core::task::{Context, Poll};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use crate::protocol::sd;
use crate::runtime::{AsyncUdpSocket, Clock, SocketFactory};

/// Errors that surface from the tokio-backed adapter.
#[derive(Debug, thiserror::Error)]
pub enum TokioAdapterError {
    /// Underlying `std::io::Error`.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// `recv_from` produced an IPv6 source — SOME/IP is IPv4-only.
    #[error("received datagram from non-IPv4 source: {0}")]
    NotIpv4(SocketAddr),
}

/// [`AsyncUdpSocket`] implementation backed by `tokio::net::UdpSocket`.
///
/// Holds the socket in an [`Arc`] so the adapter can be cloned cheaply when
/// multiple consumers need a handle (e.g. a transmitter task and a receiver
/// task in a `select!`).
#[derive(Clone, Debug)]
pub struct TokioUdpSocket {
    socket: Arc<::tokio::net::UdpSocket>,
}

impl TokioUdpSocket {
    /// Wrap an already-bound `tokio::net::UdpSocket`.
    #[must_use]
    pub fn new(socket: ::tokio::net::UdpSocket) -> Self {
        Self {
            socket: Arc::new(socket),
        }
    }

    /// Bind a fresh tokio socket to `addr`.
    ///
    /// # Errors
    /// Forwards any `std::io::Error` from `tokio::net::UdpSocket::bind`.
    pub async fn bind(addr: SocketAddrV4) -> io::Result<Self> {
        let socket = ::tokio::net::UdpSocket::bind(addr).await?;
        Ok(Self::new(socket))
    }

    /// Borrow the underlying tokio socket — useful for configuration that the
    /// trait surface intentionally omits (TTL, reuse-port, broadcast, …).
    #[must_use]
    pub fn inner(&self) -> &::tokio::net::UdpSocket {
        &self.socket
    }
}

impl AsyncUdpSocket for TokioUdpSocket {
    type Error = TokioAdapterError;

    async fn send_to(&self, buf: &[u8], dst: SocketAddrV4) -> Result<(), Self::Error> {
        let sent = self.socket.send_to(buf, dst).await?;
        if sent != buf.len() {
            return Err(TokioAdapterError::Io(io::Error::new(
                io::ErrorKind::WriteZero,
                "short UDP write",
            )));
        }
        Ok(())
    }

    fn poll_recv_from(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<(usize, SocketAddrV4), Self::Error>> {
        let mut read_buf = ::tokio::io::ReadBuf::new(buf);
        match self.socket.poll_recv_from(cx, &mut read_buf) {
            Poll::Ready(Ok(src)) => {
                let n = read_buf.filled().len();
                match src {
                    SocketAddr::V4(v4) => Poll::Ready(Ok((n, v4))),
                    SocketAddr::V6(_) => Poll::Ready(Err(TokioAdapterError::NotIpv4(src))),
                }
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e.into())),
            Poll::Pending => Poll::Pending,
        }
    }

    async fn join_multicast(&self, group: Ipv4Addr) -> Result<(), Self::Error> {
        self.socket.join_multicast_v4(group, Ipv4Addr::UNSPECIFIED)?;
        Ok(())
    }
}

/// [`Clock`] implementation backed by `tokio::time`.
///
/// Constructible as `TokioClock` — it holds no state; tokio's clock is a
/// process-global resource.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioClock;

impl Clock for TokioClock {
    type Instant = ::tokio::time::Instant;

    fn now(&self) -> Self::Instant {
        ::tokio::time::Instant::now()
    }

    async fn sleep_until(&self, deadline: Self::Instant) {
        ::tokio::time::sleep_until(deadline).await;
    }
}

/// Stateless [`SocketFactory`] backed by socket2 + tokio.
///
/// Mirrors the socket setup the previous `SocketManager::bind*` helpers
/// performed: `SO_REUSEADDR` + `SO_REUSEPORT` (unix), non-blocking mode,
/// multicast interface selection, and SD multicast group join for
/// discovery sockets.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioSocketFactory;

impl SocketFactory for TokioSocketFactory {
    type Socket = TokioUdpSocket;
    type Error = TokioAdapterError;

    async fn bind_unicast(
        &self,
        _interface: Ipv4Addr,
        port: u16,
    ) -> Result<(Self::Socket, u16), Self::Error> {
        let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);

        let socket = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        socket.set_reuse_address(true)?;
        socket.bind(&bind_addr.into())?;
        socket.set_nonblocking(true)?;
        let std_socket: std::net::UdpSocket = socket.into();
        let bound_port = std_socket.local_addr()?.port();
        let tokio_socket = ::tokio::net::UdpSocket::from_std(std_socket)?;

        Ok((TokioUdpSocket::new(tokio_socket), bound_port))
    }

    async fn bind_discovery(
        &self,
        interface: Ipv4Addr,
        multicast_loopback: bool,
    ) -> Result<Self::Socket, Self::Error> {
        let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), sd::MULTICAST_PORT);

        let socket = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        socket.set_reuse_address(true)?;
        #[cfg(unix)]
        socket.set_reuse_port(true)?;
        socket.set_multicast_if_v4(&interface)?;
        // Loopback off by default avoids the socket parsing its own
        // OfferService / FindService announcements as if they came from
        // a peer. Same-host simulator setups enable it explicitly.
        socket.set_multicast_loop_v4(multicast_loopback)?;
        socket.bind(&bind_addr.into())?;
        socket.set_nonblocking(true)?;
        let std_socket: std::net::UdpSocket = socket.into();
        let tokio_socket = ::tokio::net::UdpSocket::from_std(std_socket)?;
        tokio_socket.join_multicast_v4(sd::MULTICAST_IP, interface)?;

        Ok(TokioUdpSocket::new(tokio_socket))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::time::Duration;

    #[::tokio::test]
    async fn loopback_send_recv_roundtrip() {
        let sender = TokioUdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let receiver = TokioUdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let receiver_addr = match receiver.socket.local_addr().unwrap() {
            SocketAddr::V4(v4) => v4,
            SocketAddr::V6(_) => unreachable!("bound to IPv4"),
        };

        let payload = b"hello someip";
        sender.send_to(payload, receiver_addr).await.unwrap();

        let mut buf = [0u8; 64];
        let (n, src) = receiver.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], payload);
        assert_eq!(src.ip(), &Ipv4Addr::LOCALHOST);
    }

    #[::tokio::test]
    async fn clock_sleep_advances() {
        let clock = TokioClock;
        let start = clock.now();
        clock.sleep(Duration::from_millis(5)).await;
        let elapsed = clock.now().saturating_duration_since(start);
        assert!(elapsed >= Duration::from_millis(5));
    }

    #[::tokio::test]
    async fn factory_binds_unicast_ephemeral_port() {
        let factory = TokioSocketFactory;
        let (socket, bound_port) = factory
            .bind_unicast(Ipv4Addr::LOCALHOST, 0)
            .await
            .unwrap();
        assert!(bound_port > 0, "ephemeral bind must return a non-zero port");
        // Verify the socket is actually usable: send to it via a fresh peer.
        let peer = TokioUdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        peer.send_to(b"ping", SocketAddrV4::new(Ipv4Addr::LOCALHOST, bound_port))
            .await
            .unwrap();
        let mut buf = [0u8; 8];
        let (n, _src) = socket.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"ping");
    }
}
