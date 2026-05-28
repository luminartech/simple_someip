//! Runtime-agnostic async I/O traits for SOME/IP transports and timing.
//!
//! These traits decouple SOME/IP state machines from any specific async
//! runtime. Implementations plug in tokio (desktop), embassy (embedded async),
//! or a hand-rolled sync-polled executor (embedded firmware where the host
//! drives the event loop).
//!
//! The traits sit alongside the existing protocol primitives without
//! disturbing them: they have no implementations in this crate yet, and the
//! pre-existing tokio-backed [`client`](crate::client) /
//! [`server`](crate::server) modules continue to compile and behave
//! identically.
//!
//! ## Design choices
//!
//! - **`&self`, not `&mut self`.** Tokio's `UdpSocket` exposes `send_to` and
//!   `recv_from` on `&self` so they can be awaited concurrently inside a
//!   `select!`. Embedded implementations carry interior mutability via
//!   `RefCell`/`Cell` to match.
//! - **No `Spawn` trait.** Concurrency lives inside a single `run()` future
//!   composed with `select!`/`join!`. Callers decide whether to
//!   `tokio::spawn(run)` (desktop) or pump it from a polled executor
//!   (embedded).
//! - **Associated `Instant` on [`Clock`].** Embedded implementations use a
//!   cheap wrapping `u32` ms tick; desktop uses `tokio::time::Instant`.
//! - **`async fn` in trait.** Stable since Rust 1.75; edition 2024 here.
//!   Send/Sync bounds on returned futures are deferred until adapters
//!   exist — added when the tokio adapter (Phase 3) needs them.

// `async_fn_in_trait` warns that `Send`/`Sync` cannot be added at the trait
// declaration site. That's acceptable here: the single-task embedded path
// does not need `Send` futures, and the tokio adapter (Phase 3) will wrap
// the trait through a `trait-variant`-style shim or a `+ Send` RPITIT bound
// when the language stabilizes a path for it.
#![allow(async_fn_in_trait)]

use core::fmt::Debug;
use core::future::poll_fn;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::ops::Add;
use core::task::{Context, Poll};
use core::time::Duration;

/// Async UDP socket abstraction.
///
/// The minimum surface SOME/IP state machines need: send a datagram, receive
/// a datagram, and join a multicast group for service discovery.
///
/// Interior mutability is assumed — methods take `&self` so a single socket
/// can be driven by concurrent branches of a `select!`.
pub trait AsyncUdpSocket {
    /// Error type returned by I/O operations.
    type Error: Debug;

    /// Send `buf` as a single UDP datagram to `dst`.
    ///
    /// `Ok(())` means the transport accepted the full datagram. UDP does not
    /// fragment at this layer; implementations that cannot send the whole
    /// buffer must report an error rather than a partial send.
    ///
    /// # Errors
    /// Implementation-defined. Common causes: socket not bound, destination
    /// unreachable, transmit queue full.
    async fn send_to(&self, buf: &[u8], dst: SocketAddrV4) -> Result<(), Self::Error>;

    /// Poll-based receive primitive.
    ///
    /// Returns `Poll::Ready((n, src))` when a datagram is available — `n`
    /// bytes written to `buf`, sourced from `src`. Returns `Poll::Pending`
    /// when no datagram is ready; the implementation registers `cx`'s waker
    /// for the next arrival.
    ///
    /// Exposing the poll-based form lets callers multiplex many sockets
    /// inside a single `poll_fn` without owning per-socket futures across
    /// loop iterations — the pattern the Inner client/server `run` loop
    /// uses to drive N unicast sockets without spawning a task per socket.
    ///
    /// # Errors
    /// Implementation-defined. Common causes: socket not bound, parse
    /// failure inside a queue layer.
    fn poll_recv_from(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<(usize, SocketAddrV4), Self::Error>>;

    /// Receive a single datagram into `buf`.
    ///
    /// Default implementation awaits [`poll_recv_from`](Self::poll_recv_from).
    /// Implementations need not override unless they have a more efficient
    /// async path that avoids the `poll_fn` wrapper.
    ///
    /// Returns the number of bytes written and the source address. If `buf`
    /// is shorter than the incoming datagram, behavior is implementation
    /// defined (tokio truncates and signals via flags; embedded queue impls
    /// typically drop the datagram on enqueue).
    ///
    /// # Errors
    /// Implementation-defined. Common causes: socket not bound, decode
    /// failure in a queue layer.
    async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddrV4), Self::Error> {
        poll_fn(|cx| self.poll_recv_from(cx, buf)).await
    }

    /// Join an IPv4 multicast group.
    ///
    /// On platforms where multicast membership is managed outside the Rust
    /// transport — e.g. lwIP's IGMP layer driven from C — this is a no-op.
    ///
    /// # Errors
    /// Implementation-defined. Common cause: the underlying socket API
    /// rejected the join request.
    async fn join_multicast(&self, group: Ipv4Addr) -> Result<(), Self::Error>;
}

/// Factory for binding UDP sockets used by SOME/IP clients and servers.
///
/// Binding is split from [`AsyncUdpSocket`] so that platforms which manage
/// socket lifecycle outside of Rust (e.g. embedded targets where C owns the
/// PCB pool) can provide a factory that hands out pre-bound sockets rather
/// than calling into a kernel network stack.
pub trait SocketFactory {
    /// Concrete socket type produced by this factory.
    type Socket: AsyncUdpSocket;
    /// Error type for bind failures.
    type Error: Debug;

    /// Bind a unicast UDP socket to `interface:port`.
    ///
    /// When `port` is `0`, the implementation selects an ephemeral port; the
    /// actually-bound port is returned alongside the socket.
    ///
    /// # Errors
    /// Implementation-defined — typically address-in-use, permission denied,
    /// or pool exhaustion on embedded.
    async fn bind_unicast(
        &self,
        interface: Ipv4Addr,
        port: u16,
    ) -> Result<(Self::Socket, u16), Self::Error>;

    /// Bind a UDP socket for SOME/IP SD multicast use.
    ///
    /// Implementations must enable port reuse, select the specified
    /// `interface` for multicast egress, join the SD multicast group, and
    /// honour `multicast_loopback` (typically off; enabled only for
    /// same-host simulator + client setups).
    ///
    /// # Errors
    /// Implementation-defined.
    async fn bind_discovery(
        &self,
        interface: Ipv4Addr,
        multicast_loopback: bool,
    ) -> Result<Self::Socket, Self::Error>;
}

/// Monotonic clock abstraction.
///
/// `Instant` is implementation-defined. Embedded impls typically wrap a
/// `u32` millisecond counter; desktop impls use `tokio::time::Instant`. The
/// trait bound `Instant + Duration -> Instant` lets the default
/// [`sleep`](Self::sleep) method compute a deadline from a duration.
pub trait Clock {
    /// Implementation-defined instant type.
    type Instant: Copy + Ord + Add<Duration, Output = Self::Instant>;

    /// Read the current monotonic time.
    fn now(&self) -> Self::Instant;

    /// Yield until `deadline` has passed.
    async fn sleep_until(&self, deadline: Self::Instant);

    /// Sleep for `duration`. The default implementation composes
    /// [`now`](Self::now) with [`sleep_until`](Self::sleep_until).
    async fn sleep(&self, duration: Duration) {
        let deadline = self.now() + duration;
        self.sleep_until(deadline).await;
    }
}
