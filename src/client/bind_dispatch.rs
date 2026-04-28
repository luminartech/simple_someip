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
    ChannelFactory, E2ERegistryHandle, LocalSpawner, Spawner, TransportFactory, TransportSocket,
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
}

/// `BindDispatch` for the multi-threaded path: requires a
/// [`Spawner`] and a `Send + Sync` transport socket.
pub(super) struct SpawnerDispatch<F, S> {
    pub factory: F,
    pub spawner: S,
}

impl<MD, C, R, F, S> BindDispatch<MD, C, R> for SpawnerDispatch<F, S>
where
    MD: PayloadWireFormat + Clone + core::fmt::Debug + Send + 'static,
    C: ChannelFactory,
    R: E2ERegistryHandle,
    F: TransportFactory + Send + Sync + 'static,
    F::Socket: Send + Sync + 'static,
    for<'a> <F::Socket as TransportSocket>::SendFuture<'a>: Send,
    for<'a> <F::Socket as TransportSocket>::RecvFuture<'a>: Send,
    S: Spawner + Send + Sync + 'static,
    Result<super::socket_manager::ReceivedMessage<MD>, Error>:
        crate::transport::BoundedPooled<C, 16>,
    super::socket_manager::SendMessage<MD, C>: crate::transport::BoundedPooled<C, 16>,
    Result<(), Error>: crate::transport::OneshotPooled<C>,
{
    fn bind_discovery(
        &self,
        interface: Ipv4Addr,
        e2e_registry: R,
        session_id: u16,
        session_has_wrapped: bool,
        multicast_loopback: bool,
    ) -> impl Future<Output = Result<SocketManager<MD, C>, Error>> + '_ {
        SocketManager::<MD, C>::bind_discovery_seeded_with_transport(
            &self.factory,
            &self.spawner,
            interface,
            e2e_registry,
            session_id,
            session_has_wrapped,
            multicast_loopback,
        )
    }

    fn bind_unicast(
        &self,
        port: u16,
        e2e_registry: R,
    ) -> impl Future<Output = Result<SocketManager<MD, C>, Error>> + '_ {
        SocketManager::<MD, C>::bind_with_transport(
            &self.factory,
            &self.spawner,
            port,
            e2e_registry,
        )
    }
}

/// `BindDispatch` for the single-threaded path: requires a
/// [`LocalSpawner`] and `'static` transport socket. The socket and its
/// GAT futures are not required to be `Send`.
pub(super) struct LocalSpawnerDispatch<F, S> {
    pub factory: F,
    pub spawner: S,
}

impl<MD, C, R, F, S> BindDispatch<MD, C, R> for LocalSpawnerDispatch<F, S>
where
    MD: PayloadWireFormat + Clone + core::fmt::Debug + Send + 'static,
    C: ChannelFactory,
    R: E2ERegistryHandle,
    F: TransportFactory + 'static,
    F::Socket: 'static,
    S: LocalSpawner + 'static,
    Result<super::socket_manager::ReceivedMessage<MD>, Error>:
        crate::transport::BoundedPooled<C, 16>,
    super::socket_manager::SendMessage<MD, C>: crate::transport::BoundedPooled<C, 16>,
    Result<(), Error>: crate::transport::OneshotPooled<C>,
{
    fn bind_discovery(
        &self,
        interface: Ipv4Addr,
        e2e_registry: R,
        session_id: u16,
        session_has_wrapped: bool,
        multicast_loopback: bool,
    ) -> impl Future<Output = Result<SocketManager<MD, C>, Error>> + '_ {
        SocketManager::<MD, C>::bind_discovery_seeded_with_transport_local(
            &self.factory,
            &self.spawner,
            interface,
            e2e_registry,
            session_id,
            session_has_wrapped,
            multicast_loopback,
        )
    }

    fn bind_unicast(
        &self,
        port: u16,
        e2e_registry: R,
    ) -> impl Future<Output = Result<SocketManager<MD, C>, Error>> + '_ {
        SocketManager::<MD, C>::bind_with_transport_local(
            &self.factory,
            &self.spawner,
            port,
            e2e_registry,
        )
    }
}
