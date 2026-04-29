//! `TransportFactory` impl over embassy-net's UDP API.
//!
//! See the crate-level doc for context. This module is the scaffolding
//! introduced in phase 19a; the full impl lands in 19b.

use crate::socket::EmbassyNetSocket;

/// Caller-owned pool of UDP-socket buffer storage.
///
/// embassy-net's [`UdpSocket`](embassy_net::udp::UdpSocket) requires
/// the caller to provide RX/TX buffers and metadata arrays. To satisfy
/// `simple-someip`'s `F::Socket: 'static` bound (the run-loop spawns
/// per-socket I/O tasks), the buffers must live in `&'static` storage.
///
/// `SocketPool` declares N slots of buffer storage; the
/// [`EmbassyNetFactory`] hands each `bind()` call a fresh slot until
/// the pool is exhausted, after which `bind()` returns
/// [`simple_someip::transport::TransportError::AddressInUse`] (the
/// closest existing variant — phase 19b will introduce a dedicated
/// pool-exhaustion path or rename this).
///
/// **NB phase 19a:** the actual storage fields are deferred to 19b
/// once the embassy-net buffer-shape bring-up reveals what we need
/// (`PacketMetadata` arrays vs. the older `[u8]` form, etc.). This
/// stub exists so the `factory` module type-checks against the
/// `EmbassyNetFactory` skeleton.
#[allow(dead_code)] // populated in 19b
pub struct SocketPool<const POOL: usize, const RX_BUF: usize, const TX_BUF: usize> {
    // Storage arrays will land in 19b.
    _todo: (),
}

impl<const POOL: usize, const RX_BUF: usize, const TX_BUF: usize> SocketPool<POOL, RX_BUF, TX_BUF> {
    /// Construct an empty socket pool. Const so it can live in a
    /// `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self { _todo: () }
    }
}

impl<const POOL: usize, const RX_BUF: usize, const TX_BUF: usize> Default
    for SocketPool<POOL, RX_BUF, TX_BUF>
{
    fn default() -> Self {
        Self::new()
    }
}

/// embassy-net `TransportFactory` implementation.
///
/// Holds a reference to the embassy-net `Stack` and a `&'static`
/// [`SocketPool`] from which `bind()` allocates per-socket buffers.
///
/// **NB phase 19a:** the [`TransportFactory`](simple_someip::transport::TransportFactory)
/// trait impl lands in 19b. This skeleton exists so downstream code
/// can name the type and so the workspace integration can be
/// validated incrementally.
#[allow(dead_code)] // populated in 19b
pub struct EmbassyNetFactory<'a, const POOL: usize, const RX_BUF: usize, const TX_BUF: usize> {
    pool: &'a SocketPool<POOL, RX_BUF, TX_BUF>,
}

impl<'a, const POOL: usize, const RX_BUF: usize, const TX_BUF: usize>
    EmbassyNetFactory<'a, POOL, RX_BUF, TX_BUF>
{
    /// Build a factory borrowing from the given socket pool.
    #[must_use]
    pub fn new(pool: &'a SocketPool<POOL, RX_BUF, TX_BUF>) -> Self {
        Self { pool }
    }
}

// `EmbassyNetSocket` is the eventual associated type of the
// `TransportFactory` impl; the explicit `use` above keeps the
// import live so 19b doesn't have to reintroduce it. Without an
// active reference Rust would fire `unused_import`.
#[allow(dead_code)]
fn _phantom_socket_use() -> Option<EmbassyNetSocket> {
    None
}
