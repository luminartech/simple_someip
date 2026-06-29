//! Spawner-agnostic bind dispatch for the `Client` run-loop.
//!
//! `Inner` needs to bind two kinds of UDP sockets — the SD multicast
//! socket and per-port unicast sockets — and submit each socket's I/O
//! loop to a task spawner. Multi-threaded executors (tokio default)
//! require the spawned future to be `Send`; single-threaded executors
//! (embassy with `task-arena = 0`, tokio's `LocalSet`) accept `!Send`
//! futures via [`crate::LocalSpawner`].
//!
//! Rather than duplicating `Inner::run_future` for the two cases, we
//! abstract the bind-and-spawn step behind [`BindDispatch`]. `Inner` is
//! generic over a single `D: BindDispatch` field; the public
//! [`Client::new_with_deps`](super::Client::new_with_deps) constructs a
//! [`SpawnerDispatch`] and
//! [`Client::new_with_deps_local`](super::Client::new_with_deps_local)
//! constructs a [`LocalSpawnerDispatch`].
//!
//! The trait is intentionally crate-private — third parties extend the
//! public surface by implementing [`crate::Spawner`] or
//! [`crate::LocalSpawner`], not by writing their own `BindDispatch`.

use core::future::Future;
use core::net::Ipv4Addr;

use super::error::Error;
use super::socket_manager::SocketManager;
use crate::traits::PayloadWireFormat;
use crate::transport::{
    BufferProvider, ChannelFactory, E2ERegistryHandle, LocalSpawner, Spawner, TransportFactory,
    TransportSocket,
};

/// Crate-private bind-and-spawn abstraction shared by Send and `!Send`
/// `Client` construction paths.
pub(super) trait BindDispatch<MD, C, R>
where
    MD: PayloadWireFormat + Clone + core::fmt::Debug + Send + 'static,
    C: ChannelFactory,
    R: E2ERegistryHandle,
    Result<super::socket_manager::ReceivedMessage<MD>, Error>:
        crate::transport::BoundedPooled<C, 16>,
    super::socket_manager::SendMessage<MD, C>: crate::transport::BoundedPooled<C, 16>,
    Result<(), Error>: crate::transport::OneshotPooled<C>,
{
    /// Bind a discovery socket and submit its I/O loop to the
    /// configured task executor.
    // `async move` body (rather than `async fn`) is required: the trait
    // method returns `impl Future`, and the block must capture `&self` to
    // claim a buffer (#125) before delegating to `SocketManager::bind_*`.
    #[allow(clippy::manual_async_fn)]
    fn bind_discovery(
        &self,
        interface: Ipv4Addr,
        e2e_registry: R,
        session_id: u16,
        session_has_wrapped: bool,
        multicast_loopback: bool,
    ) -> impl Future<Output = Result<SocketManager<MD, C>, Error>> + '_;

    /// Bind a unicast socket on `port` (0 = ephemeral) and submit its
    /// I/O loop.
    fn bind_unicast(
        &self,
        port: u16,
        e2e_registry: R,
    ) -> impl Future<Output = Result<SocketManager<MD, C>, Error>> + '_;

    /// Bind a receive-only unicast service-discovery socket on the
    /// `interface` IP and submit its I/O loop. Diverts the sensor's
    /// unicast SD off the multicast discovery socket so the two SD
    /// session domains track on separate keys.
    #[allow(clippy::manual_async_fn)]
    fn bind_discovery_unicast(
        &self,
        interface: Ipv4Addr,
        e2e_registry: R,
    ) -> impl Future<Output = Result<SocketManager<MD, C>, Error>> + '_;
}

/// `BindDispatch` for the multi-threaded path: requires a
/// [`Spawner`] and a `Send + Sync` transport socket.
///
/// Carries a [`BufferProvider`] (`#125`): each `bind_*` claims one
/// socket-loop buffer from it and moves the lease into the spawned loop
/// future. The lease frees its pool slot when that future drops (i.e. when
/// the socket closes), so no explicit release is needed at eviction.
pub(super) struct SpawnerDispatch<F, S, BP> {
    pub factory: F,
    pub spawner: S,
    pub buffer_provider: BP,
}

impl<MD, C, R, F, S, BP> BindDispatch<MD, C, R> for SpawnerDispatch<F, S, BP>
where
    MD: PayloadWireFormat + Clone + core::fmt::Debug + Send + 'static,
    C: ChannelFactory,
    R: E2ERegistryHandle,
    F: TransportFactory + Send + Sync + 'static,
    F::Socket: Send + Sync + 'static,
    for<'a> F::BindFuture<'a>: Send,
    for<'a> <F::Socket as TransportSocket>::SendFuture<'a>: Send,
    for<'a> <F::Socket as TransportSocket>::RecvFuture<'a>: Send,
    S: Spawner + Send + Sync + 'static,
    BP: BufferProvider,
    Result<super::socket_manager::ReceivedMessage<MD>, Error>:
        crate::transport::BoundedPooled<C, 16>,
    super::socket_manager::SendMessage<MD, C>: crate::transport::BoundedPooled<C, 16>,
    Result<(), Error>: crate::transport::OneshotPooled<C>,
{
    // `async move` body (rather than `async fn`) is required: the trait
    // method returns `impl Future`, and the block must capture `&self` to
    // claim a buffer (#125) before delegating to `SocketManager::bind_*`.
    #[allow(clippy::manual_async_fn)]
    fn bind_discovery(
        &self,
        interface: Ipv4Addr,
        e2e_registry: R,
        session_id: u16,
        session_has_wrapped: bool,
        multicast_loopback: bool,
    ) -> impl Future<Output = Result<SocketManager<MD, C>, Error>> + '_ {
        async move {
            let buf = self
                .buffer_provider
                .claim()
                .ok_or(Error::Capacity("udp_buffer"))?;
            SocketManager::<MD, C>::bind_discovery_seeded_with_transport(
                &self.factory,
                &self.spawner,
                interface,
                e2e_registry,
                session_id,
                session_has_wrapped,
                multicast_loopback,
                buf,
            )
            .await
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn bind_unicast(
        &self,
        port: u16,
        e2e_registry: R,
    ) -> impl Future<Output = Result<SocketManager<MD, C>, Error>> + '_ {
        async move {
            let buf = self
                .buffer_provider
                .claim()
                .ok_or(Error::Capacity("udp_buffer"))?;
            SocketManager::<MD, C>::bind_with_transport(
                &self.factory,
                &self.spawner,
                port,
                e2e_registry,
                buf,
            )
            .await
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn bind_discovery_unicast(
        &self,
        interface: Ipv4Addr,
        e2e_registry: R,
    ) -> impl Future<Output = Result<SocketManager<MD, C>, Error>> + '_ {
        async move {
            let buf = self
                .buffer_provider
                .claim()
                .ok_or(Error::Capacity("udp_buffer"))?;
            SocketManager::<MD, C>::bind_discovery_unicast_with_transport(
                &self.factory,
                &self.spawner,
                interface,
                e2e_registry,
                buf,
            )
            .await
        }
    }
}

/// `BindDispatch` for the single-threaded path: requires a
/// [`LocalSpawner`] and `'static` transport socket. The socket and its
/// GAT futures are not required to be `Send`.
///
/// Carries a [`BufferProvider`] for the same reason as [`SpawnerDispatch`]:
/// each `bind_*` claims one socket-loop buffer and moves the lease into the
/// spawned loop future, which frees the slot on drop.
pub(super) struct LocalSpawnerDispatch<F, S, BP> {
    pub factory: F,
    pub spawner: S,
    pub buffer_provider: BP,
}

impl<MD, C, R, F, S, BP> BindDispatch<MD, C, R> for LocalSpawnerDispatch<F, S, BP>
where
    MD: PayloadWireFormat + Clone + core::fmt::Debug + Send + 'static,
    C: ChannelFactory,
    R: E2ERegistryHandle,
    F: TransportFactory + 'static,
    F::Socket: 'static,
    S: LocalSpawner + 'static,
    BP: BufferProvider,
    Result<super::socket_manager::ReceivedMessage<MD>, Error>:
        crate::transport::BoundedPooled<C, 16>,
    super::socket_manager::SendMessage<MD, C>: crate::transport::BoundedPooled<C, 16>,
    Result<(), Error>: crate::transport::OneshotPooled<C>,
{
    // `async move` body (rather than `async fn`) is required: the trait
    // method returns `impl Future`, and the block must capture `&self` to
    // claim a buffer (#125) before delegating to `SocketManager::bind_*`.
    #[allow(clippy::manual_async_fn)]
    fn bind_discovery(
        &self,
        interface: Ipv4Addr,
        e2e_registry: R,
        session_id: u16,
        session_has_wrapped: bool,
        multicast_loopback: bool,
    ) -> impl Future<Output = Result<SocketManager<MD, C>, Error>> + '_ {
        async move {
            let buf = self
                .buffer_provider
                .claim()
                .ok_or(Error::Capacity("udp_buffer"))?;
            SocketManager::<MD, C>::bind_discovery_seeded_with_transport_local(
                &self.factory,
                &self.spawner,
                interface,
                e2e_registry,
                session_id,
                session_has_wrapped,
                multicast_loopback,
                buf,
            )
            .await
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn bind_unicast(
        &self,
        port: u16,
        e2e_registry: R,
    ) -> impl Future<Output = Result<SocketManager<MD, C>, Error>> + '_ {
        async move {
            let buf = self
                .buffer_provider
                .claim()
                .ok_or(Error::Capacity("udp_buffer"))?;
            SocketManager::<MD, C>::bind_with_transport_local(
                &self.factory,
                &self.spawner,
                port,
                e2e_registry,
                buf,
            )
            .await
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn bind_discovery_unicast(
        &self,
        interface: Ipv4Addr,
        e2e_registry: R,
    ) -> impl Future<Output = Result<SocketManager<MD, C>, Error>> + '_ {
        async move {
            let buf = self
                .buffer_provider
                .claim()
                .ok_or(Error::Capacity("udp_buffer"))?;
            SocketManager::<MD, C>::bind_discovery_unicast_with_transport_local(
                &self.factory,
                &self.spawner,
                interface,
                e2e_registry,
                buf,
            )
            .await
        }
    }
}
