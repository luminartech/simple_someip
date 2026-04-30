//! Tokio + socket2 implementation of the [`crate::transport`] traits.
//!
//! This is the default `std` backend. [`TokioTransport`] constructs
//! configured [`TokioSocket`]s via `socket2` for bind-time options (reuse,
//! multicast interface, multicast loop) and converts them to
//! [`tokio::net::UdpSocket`] for the async I/O loop. [`TokioTimer`] is a
//! thin wrapper over `tokio::time::sleep`.
//!
//! Gated behind `#[cfg(any(feature = "client-tokio", feature = "server-tokio"))]` —
//! the `client-tokio` and `server-tokio` features are exactly the ones
//! that pull in `tokio` and `socket2`, so no new dependency edge is
//! introduced.
//!
//! # Example
//!
//! ```no_run
//! # #[cfg(any(feature = "client-tokio", feature = "server-tokio"))]
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
use core::pin::Pin;
use core::task::{Context, Poll};
use core::time::Duration;
use std::net::{IpAddr, SocketAddr};
use tokio::io::ReadBuf;
use tokio::net::UdpSocket;

use crate::transport::{
    ChannelFactory, IoErrorKind, MpscRecv, MpscSend, OneshotCancelled, OneshotRecv, OneshotSend,
    ReceivedDatagram, SocketOptions, Timer, TransportError, TransportFactory, TransportSocket,
    UnboundedRecv, UnboundedSend,
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

/// Named future returned by [`TokioTransport::bind`].
///
/// `socket2::Socket::bind` is synchronous, so the body runs to
/// completion on the first poll; the named struct exists only to
/// satisfy the [`TransportFactory::BindFuture`] GAT on stable Rust
/// without TAIT. Auto-derives `Send`.
pub struct TokioBindFuture {
    addr: SocketAddrV4,
    options: SocketOptions,
}

impl Future for TokioBindFuture {
    type Output = Result<TokioSocket, TransportError>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let addr = self.addr;
        let options = self.options;
        Poll::Ready(bind_with_options(addr, options).map_err(|e| map_io_error(&e)))
    }
}

impl TransportFactory for TokioTransport {
    type Socket = TokioSocket;
    type BindFuture<'a> = TokioBindFuture;

    fn bind<'a>(&'a self, addr: SocketAddrV4, options: &'a SocketOptions) -> Self::BindFuture<'a> {
        TokioBindFuture {
            addr,
            options: *options,
        }
    }
}

/// Named future returned by [`TokioSocket::send_to`].
///
/// Drives [`tokio::net::UdpSocket::poll_send_to`] directly so the GAT
/// associated type ([`TransportSocket::SendFuture`]) can be named on
/// stable Rust without heap-allocating a `futures::future::BoxFuture`
/// per datagram. Auto-derives `Send`.
pub struct SendTo<'a> {
    socket: &'a UdpSocket,
    buf: &'a [u8],
    target: SocketAddr,
}

impl Future for SendTo<'_> {
    type Output = Result<(), TransportError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.socket.poll_send_to(cx, self.buf, self.target) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(_n)) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(map_io_error(&e))),
        }
    }
}

/// Named future returned by [`TokioSocket::recv_from`].
///
/// Drives [`tokio::net::UdpSocket::poll_recv_from`] directly so the GAT
/// associated type ([`TransportSocket::RecvFuture`]) can be named on
/// stable Rust without heap-allocating a `futures::future::BoxFuture`
/// per datagram. Auto-derives `Send`.
pub struct RecvFrom<'a> {
    socket: &'a UdpSocket,
    buf: &'a mut [u8],
}

impl Future for RecvFrom<'_> {
    type Output = Result<ReceivedDatagram, TransportError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // No self-references; safe to project to &mut Self.
        let me = self.get_mut();
        let mut read_buf = ReadBuf::new(me.buf);
        match me.socket.poll_recv_from(cx, &mut read_buf) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(map_io_error(&e))),
            Poll::Ready(Ok(src)) => {
                let n = read_buf.filled().len();
                let source = match src {
                    SocketAddr::V4(v4) => v4,
                    // SOME/IP is IPv4-only; an IPv6 source on our socket is
                    // either impossible (v4 bind) or a misconfiguration.
                    SocketAddr::V6(_) => return Poll::Ready(Err(TransportError::Unsupported)),
                };
                // Caveat: `tokio::net::UdpSocket::poll_recv_from` silently
                // truncates when the caller's `buf` is smaller than the
                // datagram and returns only the bytes that fit — it does
                // NOT expose a truncation flag. Surfacing a reliable
                // `truncated: bool` here would require a platform-specific
                // `recvmsg`/MSG_TRUNC path (libc + unsafe), which is
                // deferred for now. Until then, this field is always
                // `false` for the Tokio backend; callers must not rely on
                // it for truncation detection. This is documented on
                // `ReceivedDatagram::truncated`'s field doc.
                Poll::Ready(Ok(ReceivedDatagram {
                    bytes_received: n,
                    source,
                    truncated: false,
                }))
            }
        }
    }
}

impl TransportSocket for TokioSocket {
    type SendFuture<'a> = SendTo<'a>;
    type RecvFuture<'a> = RecvFrom<'a>;

    fn send_to<'a>(&'a self, buf: &'a [u8], target: SocketAddrV4) -> Self::SendFuture<'a> {
        SendTo {
            socket: &self.inner,
            buf,
            target: SocketAddr::V4(target),
        }
    }

    fn recv_from<'a>(&'a self, buf: &'a mut [u8]) -> Self::RecvFuture<'a> {
        RecvFrom {
            socket: &self.inner,
            buf,
        }
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

/// Named future returned by [`TokioTimer::sleep`].
///
/// Wraps `tokio::time::Sleep` so the [`Timer::SleepFuture`] GAT can be
/// named on stable Rust. Auto-derives `Send`.
pub struct TokioSleep {
    inner: tokio::time::Sleep,
}

impl Future for TokioSleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: structural pinning of the `inner` Sleep field. We never
        // move out of `inner` and we project pin through it consistently.
        let inner = unsafe { self.map_unchecked_mut(|s| &mut s.inner) };
        inner.poll(cx).map(|()| ())
    }
}

impl Timer for TokioTimer {
    type SleepFuture<'a> = TokioSleep;

    fn sleep(&self, duration: Duration) -> Self::SleepFuture<'_> {
        TokioSleep {
            inner: tokio::time::sleep(duration),
        }
    }
}

/// Wraps a `Future` so that any panic during `poll` is logged via
/// `tracing::error!` and the future then resolves cleanly. Lets
/// `TokioSpawner::spawn` use exactly **one** tokio task per call
/// instead of pairing each work future with a `JoinHandle`-watcher
/// task — the prior watcher-pair pattern doubled task count and
/// added `UNICAST_SOCKETS_CAP` extra tasks per `Client`.
struct PanicLoggingFut<F> {
    inner: F,
}

impl<F: Future<Output = ()>> Future for PanicLoggingFut<F> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: structural pinning of `inner`. We never move out of
        // `inner` and project pin through it consistently.
        let inner = unsafe { self.map_unchecked_mut(|s| &mut s.inner) };
        // `AssertUnwindSafe` is sound here because:
        //  - if `inner.poll` panics, the future is logged-and-dropped
        //    and never polled again, so any half-mutated state is
        //    discarded with the future itself.
        //  - the spawned task is the sole owner of this future; no
        //    aliasing observer can witness inconsistent state.
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| inner.poll(cx))) {
            Ok(poll) => poll,
            Err(payload) => {
                let msg = panic_payload_str(&payload);
                tracing::error!(
                    panic_message = msg,
                    "spawned task panicked; channels will close",
                );
                // The panicking poll's borrows are gone (caught
                // unwind dropped the stack frame), so the dependent
                // `Error::SocketClosedUnexpectedly` will surface on
                // the receiver side as the caller's channel ends
                // drop. Resolve the future cleanly so tokio doesn't
                // also flag this as an aborted task.
                Poll::Ready(())
            }
        }
    }
}

impl crate::transport::Spawner for TokioSpawner {
    fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        // Drop the returned `JoinHandle` — per-socket loops run until
        // their owning `SocketManager` drops its channel ends, at
        // which point the future completes naturally. Panic-logging
        // is built into the wrapper; one task per spawn.
        drop(tokio::spawn(PanicLoggingFut { inner: future }));
    }
}

/// Best-effort extraction of a printable message from a panic payload.
fn panic_payload_str(payload: &std::boxed::Box<dyn std::any::Any + Send>) -> &str {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        s
    } else if let Some(s) = payload.downcast_ref::<std::string::String>() {
        s.as_str()
    } else {
        "<non-string panic payload>"
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
    // Apply the multicast-loop flag whenever the caller is doing
    // multicast (interface configured) OR explicitly asked for
    // loop=true. Skipping the syscall only when both are unset avoids
    // a no-op call on plain-unicast sockets while still honoring an
    // explicit caller request.
    if let Some(loop_v4) = options.multicast_loop_v4 {
        raw.set_multicast_loop_v4(loop_v4)?;
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
        K::WouldBlock => TransportError::Io(IoErrorKind::WouldBlock),
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

// ── TokioChannels ─────────────────────────────────────────────────────────

/// [`ChannelFactory`] implementation backed by `tokio::sync::mpsc` and
/// `tokio::sync::oneshot`. This is the default channel backend for `std +
/// tokio` builds (active when the `client-tokio` or `server-tokio` feature
/// is enabled — the bare `client` / `server` features supply the
/// trait-surface only and require a caller-provided `ChannelFactory`).
#[derive(Clone, Copy)]
pub struct TokioChannels;

// Newtype wrappers are needed because Rust does not allow implementing a
// foreign trait on a foreign type (orphan rule). Wrapping the tokio receiver
// types lets us impl OneshotRecv / UnboundedRecv on them.

/// Newtype wrapping `tokio::sync::oneshot::Receiver<T>` to implement
/// [`OneshotRecv`].
pub struct TokioOneshotReceiver<T>(pub(crate) tokio::sync::oneshot::Receiver<T>);

/// Newtype wrapping `tokio::sync::mpsc::UnboundedReceiver<T>` to implement
/// [`UnboundedRecv`].
pub struct TokioUnboundedReceiver<T>(pub(crate) tokio::sync::mpsc::UnboundedReceiver<T>);

impl<T: Send + 'static> OneshotSend<T> for tokio::sync::oneshot::Sender<T> {
    fn send(self, value: T) -> Result<(), T> {
        tokio::sync::oneshot::Sender::send(self, value)
    }
}

impl<T: Send + 'static> OneshotRecv<T> for TokioOneshotReceiver<T> {
    async fn recv(self) -> Result<T, OneshotCancelled> {
        self.0.await.map_err(|_| OneshotCancelled)
    }
}

impl<T: Send + 'static> MpscSend<T> for tokio::sync::mpsc::Sender<T> {
    async fn send(&self, value: T) -> Result<(), ()> {
        tokio::sync::mpsc::Sender::send(self, value)
            .await
            .map_err(|_| ())
    }
}

impl<T: Send + 'static> MpscRecv<T> for tokio::sync::mpsc::Receiver<T> {
    fn recv(&mut self) -> impl Future<Output = Option<T>> + Send + '_ {
        self.recv()
    }

    fn poll_recv(&mut self, cx: &mut core::task::Context<'_>) -> core::task::Poll<Option<T>> {
        self.poll_recv(cx)
    }
}

impl<T: Send + 'static> UnboundedSend<T> for tokio::sync::mpsc::UnboundedSender<T> {
    fn send_now(&self, value: T) -> Result<(), T> {
        self.send(value).map_err(|e| e.0)
    }
}

impl<T: Send + 'static> UnboundedRecv<T> for TokioUnboundedReceiver<T> {
    fn recv(&mut self) -> impl Future<Output = Option<T>> + Send + '_ {
        self.0.recv()
    }
}

impl ChannelFactory for TokioChannels {
    type OneshotSender<T: Send + 'static> = tokio::sync::oneshot::Sender<T>;
    type OneshotReceiver<T: Send + 'static> = TokioOneshotReceiver<T>;

    // Tokio's `mpsc` channels store capacity at runtime, so the
    // const-generic `N` is informational only — it does not affect
    // the stored type. Embassy-sync's impl uses `N` differently (see
    // `embassy_channels`).
    type BoundedSender<T: Send + 'static, const N: usize> = tokio::sync::mpsc::Sender<T>;
    type BoundedReceiver<T: Send + 'static, const N: usize> = tokio::sync::mpsc::Receiver<T>;

    type UnboundedSender<T: Send + 'static> = tokio::sync::mpsc::UnboundedSender<T>;
    type UnboundedReceiver<T: Send + 'static> = TokioUnboundedReceiver<T>;

    // The three constructor methods (`oneshot`, `bounded`, `unbounded`)
    // use the trait's default bodies, which delegate to the per-`T`
    // `*Pooled<TokioChannels>` blanket impls below. Tokio has a single
    // shared allocator, so every `T: Send + 'static` is poolable; the
    // blanket impls capture that.
}

// Blanket `*Pooled` impls for every `T: Send + 'static` against
// `TokioChannels`. Tokio has a single shared allocator and so does not
// need per-`T` storage — each call constructs a fresh channel.
impl<T: Send + 'static> crate::transport::OneshotPooled<TokioChannels> for T {
    fn oneshot_pair() -> (
        <TokioChannels as ChannelFactory>::OneshotSender<T>,
        <TokioChannels as ChannelFactory>::OneshotReceiver<T>,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (tx, TokioOneshotReceiver(rx))
    }
}

impl<T: Send + 'static, const N: usize> crate::transport::BoundedPooled<TokioChannels, N> for T {
    fn bounded_pair() -> (
        <TokioChannels as ChannelFactory>::BoundedSender<T, N>,
        <TokioChannels as ChannelFactory>::BoundedReceiver<T, N>,
    ) {
        tokio::sync::mpsc::channel(N)
    }
}

impl<T: Send + 'static> crate::transport::UnboundedPooled<TokioChannels> for T {
    fn unbounded_pair() -> (
        <TokioChannels as ChannelFactory>::UnboundedSender<T>,
        <TokioChannels as ChannelFactory>::UnboundedReceiver<T>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (tx, TokioUnboundedReceiver(rx))
    }
}

// ── EmbassySyncChannels (extracted) ──────────────────────────────────────
//
// The bare-metal `ChannelFactory` impl previously lived here as a sub-
// module. The `tokio_transport` module is now gated to `client-tokio` /
// `server-tokio`, so a `--features client,bare_metal` build without tokio
// could no longer reach `EmbassySyncChannels`. The impl has been moved to
// `crate::embassy_channels` (gated by `feature = "embassy_channels"`) so
// it is reachable from any client build.

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
        // Guards against a regression where `multicast_loop_v4` was
        // silently ignored on a multicast bind and the socket kept the
        // OS default, diverging from the explicit request.
        // `bind_with_options` only applies `set_multicast_loop_v4` when
        // `multicast_if_v4` is `Some` (a plain-unicast bind has no
        // meaningful multicast-loop setting), so this test always pairs
        // the loop flag with a multicast interface.
        let factory = TokioTransport;

        let opts_off = SocketOptions {
            multicast_loop_v4: Some(false),
            multicast_if_v4: Some(Ipv4Addr::LOCALHOST),
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
            multicast_loop_v4: Some(true),
            multicast_if_v4: Some(Ipv4Addr::LOCALHOST),
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
