//! Loopback integration tests.
//!
//! Two `embassy_net::Stack` instances bridged by an in-memory
//! `LoopbackDriver` pair (no kernel TUN device, no privileges
//! required). Validates the `simple-someip-embassy-net` adapter and
//! the `Server` `SocketHandle` abstraction against a real
//! `embassy_net::Stack`:
//!
//! * **`adapter_udp_roundtrip`** — bind two `EmbassyNetSocket`s,
//!   one per stack, send a UDP datagram from A to B, assert
//!   byte-equality + source-address. Tightest test of `bind` /
//!   `send_to` / `recv_from` / `local_addr` end-to-end.
//! * **`client_receives_server_sd_announcement`** — wire a real
//!   `simple_someip::Server` on stack A with `run_with_buffers`
//!   (the `!Send` path) and a real `simple_someip::Client` on
//!   stack B with `Client::new_with_deps_local`. Assert the SD
//!   multicast `OfferService` propagates through the loopback and
//!   reaches the Client's update stream.
//! * **`client_send_request_server_runloop_stable`** — passive
//!   Server on stack A, Client on stack B drives `add_endpoint` +
//!   `send_to_service` to push a SOME/IP request through the
//!   embassy-net loopback. Asserts the request serializes,
//!   transits, and lands on the Server's run-loop without
//!   panicking. (No response assertion — `simple_someip::Server`
//!   exposes no public request-handler API, matching the
//!   parent-crate reference test.)
//!
//! Runtime: `#[tokio::test(flavor = "current_thread")]` plus a
//! `LocalSet` driving the per-stack `spawn_local` runners.
//! `Stack<LoopbackDriver>` is `!Sync` (RefCell internals), so
//! `Stack::run()` is `!Send` — multi-threaded `tokio::spawn` does
//! not type-check. The same constraint propagates through
//! `EmbassyNetSocket` and forces the `_local` Client paths plus
//! `Server::run_with_buffers` (no `Send` bound).

use core::net::{Ipv4Addr, SocketAddrV4};
use core::task::{Context, Waker};
use core::time::Duration;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use embassy_net::driver::{Capabilities, Driver, HardwareAddress, LinkState, RxToken, TxToken};
use embassy_net::{Config, Stack, StackResources, StaticConfigV4};

use simple_someip::static_channels::BufferPool;
use simple_someip::transport::{
    SocketOptions, StaticBufferProvider, TransportFactory, TransportSocket,
};
use simple_someip_embassy_net::{EmbassyNetFactory, LINK_MTU, SocketPool};

// ── LoopbackDriver pair ──────────────────────────────────────────────
//
// A `Pipe` is a one-directional, in-memory packet queue with a
// receiver-side `Waker` slot. `LoopbackDriver` holds two `Pipe`s:
// `rx` (we read from this — peer's `tx`) and `tx` (we write here —
// peer's `rx`). On `transmit` we push and wake the peer's reader;
// on `receive` we pop, registering our own waker into `rx.waker` if
// the queue is empty so that a future peer `transmit` re-polls us.

/// One-direction in-memory packet queue with a waker for the reader
/// side. Wrapped in `Arc` so both ends of the loopback pair share
/// it: A's `tx` is the same `Pipe` as B's `rx`.
#[derive(Default)]
struct Pipe {
    queue: Mutex<VecDeque<Vec<u8>>>,
    /// Waker the reader registered (via `LoopbackDriver::receive`)
    /// to be notified when a new frame arrives.
    waker: Mutex<Option<Waker>>,
}

impl Pipe {
    fn push(&self, packet: Vec<u8>) {
        self.queue.lock().unwrap().push_back(packet);
        if let Some(w) = self.waker.lock().unwrap().take() {
            w.wake();
        }
    }

    fn pop(&self) -> Option<Vec<u8>> {
        self.queue.lock().unwrap().pop_front()
    }

    fn register_waker(&self, w: &Waker) {
        let mut slot = self.waker.lock().unwrap();
        // Only update if the stored waker would not wake the same
        // task — saves churn when the executor re-polls without a
        // yield in between.
        match slot.as_ref() {
            Some(existing) if existing.will_wake(w) => {}
            _ => *slot = Some(w.clone()),
        }
    }
}

/// In-memory `embassy-net` `Driver` for one side of a loopback
/// pair. Pushes frames into `tx` (the peer's `rx`) and pops from
/// `rx` (the peer's `tx`).
struct LoopbackDriver {
    rx: Arc<Pipe>,
    tx: Arc<Pipe>,
}

impl LoopbackDriver {
    /// Build a pair of drivers bridged via two shared `Pipe`s. The
    /// returned tuple is `(side_a, side_b)`; whatever `side_a`
    /// transmits, `side_b` receives, and vice versa.
    fn pair() -> (Self, Self) {
        let a_to_b = Arc::new(Pipe::default());
        let b_to_a = Arc::new(Pipe::default());
        let a = LoopbackDriver {
            rx: Arc::clone(&b_to_a),
            tx: Arc::clone(&a_to_b),
        };
        let b = LoopbackDriver {
            rx: a_to_b,
            tx: b_to_a,
        };
        (a, b)
    }
}

impl Driver for LoopbackDriver {
    type RxToken<'a> = LoopbackRxToken;
    type TxToken<'a> = LoopbackTxToken;

    fn receive(&mut self, cx: &mut Context) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if let Some(packet) = self.rx.pop() {
            return Some((
                LoopbackRxToken { packet },
                LoopbackTxToken {
                    tx: Arc::clone(&self.tx),
                },
            ));
        }
        // Queue empty — register so peer's `transmit` wakes us.
        // Re-poll once after registering to close the obvious race
        // (peer pushed between our pop and our registration).
        self.rx.register_waker(cx.waker());
        if let Some(packet) = self.rx.pop() {
            return Some((
                LoopbackRxToken { packet },
                LoopbackTxToken {
                    tx: Arc::clone(&self.tx),
                },
            ));
        }
        None
    }

    fn transmit(&mut self, _cx: &mut Context) -> Option<Self::TxToken<'_>> {
        // Loopback never blocks on tx — the queue is unbounded. A
        // production driver would gate this on tx-ring availability.
        Some(LoopbackTxToken {
            tx: Arc::clone(&self.tx),
        })
    }

    fn link_state(&mut self, _cx: &mut Context) -> LinkState {
        LinkState::Up
    }

    fn capabilities(&self) -> Capabilities {
        let mut caps = Capabilities::default();
        // `medium-ip` smoltcp feature: raw IP packets, no Ethernet
        // frame, paired with `HardwareAddress::Ip` below.
        caps.max_transmission_unit = LINK_MTU;
        caps.max_burst_size = None;
        caps
    }

    fn hardware_address(&self) -> HardwareAddress {
        // `Ip` medium: skip ARP, skip Ethernet header. Two stacks
        // talk pure IP at each other across the loopback. This
        // matches the medium most lwIP / vendor-stack consumers
        // will run, and avoids needing a fake MAC + ARP exchange
        // for the test to make progress.
        HardwareAddress::Ip
    }
}

struct LoopbackRxToken {
    packet: Vec<u8>,
}

impl RxToken for LoopbackRxToken {
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        f(&mut self.packet)
    }
}

struct LoopbackTxToken {
    tx: Arc<Pipe>,
}

impl TxToken for LoopbackTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0u8; len];
        let r = f(&mut buf);
        self.tx.push(buf);
        r
    }
}

// ── Stack scaffolding ────────────────────────────────────────────────
//
// embassy-net's `Stack::new` requires `&'static mut StackResources`,
// and `EmbassyNetFactory::new` requires `&'static Stack<D>`. Tests
// materialize both via `Box::leak` — host-only, fresh per test.

const STACK_SOCKETS: usize = 8;

/// Build a stack on `ip/24` with our `LoopbackDriver`. Returns a
/// `&'static Stack<LoopbackDriver>` ready for `EmbassyNetFactory`
/// and a separately-leaked future to `tokio::spawn` for the
/// stack's run loop.
fn build_stack(driver: LoopbackDriver, ip: Ipv4Addr, seed: u64) -> &'static Stack<LoopbackDriver> {
    let resources: &'static mut StackResources<STACK_SOCKETS> =
        Box::leak(Box::new(StackResources::<STACK_SOCKETS>::new()));
    let config = Config::ipv4_static(StaticConfigV4 {
        address: embassy_net::Ipv4Cidr::new(embassy_net::Ipv4Address(ip.octets()), 24),
        gateway: None,
        // `Default::default()` picks up embassy-net's bundled
        // `heapless::Vec` version rather than this adapter's
        // (different majors don't share types).
        dns_servers: Default::default(),
    });
    Box::leak(Box::new(Stack::new(driver, config, resources, seed)))
}

// ── Stack pair convenience ──────────────────────────────────────────
//
// embassy-net's `Stack<D>` holds a `RefCell<Inner<D>>` for smoltcp
// state, so it is `!Sync`. That makes the `Stack::run()` future
// `!Send` (it captures `&'static Stack<D>`), which forces a
// single-threaded test runtime: `#[tokio::test(flavor =
// "current_thread")]` plus a `LocalSet` that drives the per-stack
// `spawn_local` runners. The same constraint forces the SOME/IP
// integration to use `Client::new_with_deps_local` (the
// `LocalSpawner`-trait counterpart for !Send-bound transports).

const IP_A: Ipv4Addr = Ipv4Addr::new(169, 254, 1, 1);
const IP_B: Ipv4Addr = Ipv4Addr::new(169, 254, 1, 2);
const SEED_A: u64 = 0x1111_2222_3333_4444;
const SEED_B: u64 = 0x5555_6666_7777_8888;

// ── Adapter-level UDP roundtrip test ────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn adapter_udp_roundtrip() {
    let (drv_a, drv_b) = LoopbackDriver::pair();
    let stack_a = build_stack(drv_a, IP_A, SEED_A);
    let stack_b = build_stack(drv_b, IP_B, SEED_B);

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            tokio::task::spawn_local(async move { stack_a.run().await });
            tokio::task::spawn_local(async move { stack_b.run().await });

            let pool_a: &'static SocketPool<2, LINK_MTU, LINK_MTU> =
                Box::leak(Box::new(SocketPool::new()));
            let pool_b: &'static SocketPool<2, LINK_MTU, LINK_MTU> =
                Box::leak(Box::new(SocketPool::new()));
            let factory_a = EmbassyNetFactory::new(stack_a, pool_a);
            let factory_b = EmbassyNetFactory::new(stack_b, pool_b);

            let opts = SocketOptions::default();
            let sock_a = factory_a
                .bind(SocketAddrV4::new(IP_A, 30501), &opts)
                .await
                .expect("bind A");
            let sock_b = factory_b
                .bind(SocketAddrV4::new(IP_B, 30502), &opts)
                .await
                .expect("bind B");

            let payload = b"phase-19e: hello-from-a";
            let dest_b = SocketAddrV4::new(IP_B, 30502);
            let mut recv_buf = [0u8; 1500];

            let send_a = sock_a.send_to(payload, dest_b);
            let recv_b = sock_b.recv_from(&mut recv_buf);
            // `current_thread` flavor: the LocalSet drives the
            // spawned stack runners between awaits. Joining
            // send/recv concurrently lets the executor interleave
            // the stack-side I/O with the test's progress.
            let (send_res, recv_res) = tokio::time::timeout(Duration::from_secs(5), async move {
                tokio::join!(send_a, recv_b)
            })
            .await
            .expect("a→b roundtrip timed out");

            send_res.expect("send_to a→b");
            let datagram = recv_res.expect("recv from a→b");
            assert_eq!(datagram.bytes_received, payload.len());
            assert!(!datagram.truncated);
            assert_eq!(&recv_buf[..datagram.bytes_received], payload);
            assert_eq!(datagram.source.ip(), &IP_A);
            assert_eq!(datagram.source.port(), 30501);
        })
        .await;
}

/// Exhaust a tiny `SocketPool` so the next `bind` returns
/// `TransportError::AddressInUse`. Covers the pool-exhausted fallback
/// path inside `EmbassyNetFactory::bind`; without an explicit test
/// that branch is dead code per coverage.
#[tokio::test(flavor = "current_thread")]
async fn factory_bind_returns_address_in_use_when_pool_exhausted() {
    let (drv_a, _drv_b) = LoopbackDriver::pair();
    let stack_a = build_stack(drv_a, IP_A, SEED_A);

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            tokio::task::spawn_local(async move { stack_a.run().await });

            // Pool of size 1: claim the only slot, then verify a
            // second bind fails with AddressInUse.
            let pool: &'static SocketPool<1, LINK_MTU, LINK_MTU> =
                Box::leak(Box::new(SocketPool::new()));
            let factory = EmbassyNetFactory::new(stack_a, pool);
            let opts = SocketOptions::default();
            let _hold = factory
                .bind(SocketAddrV4::new(IP_A, 41000), &opts)
                .await
                .expect("first bind on a fresh size-1 pool must succeed");
            let second = factory.bind(SocketAddrV4::new(IP_A, 41001), &opts).await;
            match second {
                Err(simple_someip::transport::TransportError::AddressInUse) => {}
                Err(other) => panic!(
                    "second bind on exhausted pool must yield AddressInUse, got Err({other:?})"
                ),
                Ok(_) => panic!("second bind on exhausted pool must fail"),
            }
        })
        .await;
}

/// Bind via the factory using `0.0.0.0` (wildcard IP) to cover the
/// `addr.ip().is_unspecified()` branch in `EmbassyNetFactory::bind`
/// that translates wildcard IPs to embassy-net's `addr: None`
/// listen-on-any-interface mode.
#[tokio::test(flavor = "current_thread")]
async fn factory_bind_accepts_wildcard_ip() {
    let (drv_a, _drv_b) = LoopbackDriver::pair();
    let stack_a = build_stack(drv_a, IP_A, SEED_A);

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            tokio::task::spawn_local(async move { stack_a.run().await });

            let pool: &'static SocketPool<1, LINK_MTU, LINK_MTU> =
                Box::leak(Box::new(SocketPool::new()));
            let factory = EmbassyNetFactory::new(stack_a, pool);
            let opts = SocketOptions::default();
            let sock = factory
                .bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 41100), &opts)
                .await
                .expect("wildcard bind must succeed");
            // `local_addr` reflects the wildcard IP back to the
            // caller (we record the caller's intent verbatim since
            // embassy-net's `endpoint().addr` is `None` here and we
            // have nothing better to substitute).
            let local = sock.local_addr().expect("local_addr");
            assert_eq!(*local.ip(), Ipv4Addr::UNSPECIFIED);
            assert_eq!(local.port(), 41100);
        })
        .await;
}

// ── SOME/IP Client+Server harness ───────────────────────────────────
//
// Adds a real `simple_someip::Client` + `simple_someip::Server` on
// top of the two-stack loopback, exercising the bare-metal
// constructors over `EmbassyNetFactory`. The `SocketHandle`
// abstraction lets `Server` accept `Arc<EmbassyNetSocket>` as its
// `H` parameter even though `EmbassyNetSocket` is `!Sync`.
//
// Both tests run on `flavor = "current_thread"` + `LocalSet` because:
//   - `Stack<LoopbackDriver>` is `!Sync` (RefCell internals), so
//     `Stack::run()` is `!Send`. Multi-thread `tokio::spawn`
//     rejects it.
//   - `EmbassyNetSocket` is `!Sync` for the same reason. The
//     Client's run-future captures `&self.unicast_socket`-style
//     borrows across awaits, which makes that future `!Send`. So
//     the spawner must be `LocalSpawner`, not `Spawner`. The
//     Client-side path that accepts a `LocalSpawner` is
//     `Client::new_with_deps_local`, which has shipped since phase
//     17.

use core::pin::Pin;
use core::task::Poll;
use std::sync::RwLock;

use simple_someip::PayloadWireFormat;
use simple_someip::client::Error as ClientError;
use simple_someip::client::{ClientUpdate, ControlMessage, ReceivedMessage, SendMessage};
use simple_someip::define_static_channels;
use simple_someip::e2e::E2ERegistry;
use simple_someip::protocol::sd::RebootFlag;
use simple_someip::protocol::{
    Header as SomeIpHeader, Message, MessageId, MessageType, MessageTypeField, ReturnCode,
};
use simple_someip::server::{ServerConfig, SubscribeError, Subscriber, SubscriptionHandle};
use simple_someip::transport::{LocalSpawner, Timer};
use simple_someip::{Client, ClientDeps, RawPayload, Server, ServerDeps};

// ── Static-pool channels ────────────────────────────────────────────
//
// Sized small for the witness; production firmware would size to the
// workload's high-water mark. The macro generates a `LoopbackTestChannels`
// type that implements `ChannelFactory` plus all the `*Pooled` traits
// the Client engine asks for.

define_static_channels! {
    name: LoopbackTestChannels,
    oneshot: [
        (Result<(), ClientError>, 16),
        (Result<RawPayload, ClientError>, 8),
        (Result<RebootFlag, ClientError>, 8),
    ],
    bounded: [
        ((ControlMessage<RawPayload, LoopbackTestChannels>, 4), 4),
        ((SendMessage<RawPayload, LoopbackTestChannels>, 16), 8),
        ((Result<ReceivedMessage<RawPayload>, ClientError>, 16), 8),
    ],
    unbounded: [
        (ClientUpdate<RawPayload>, 4),
    ],
}

// ── Spawner + Timer + Subscriptions harness ─────────────────────────

/// `LocalSpawner` impl backed by `tokio::task::spawn_local`. Drops
/// the `JoinHandle` — fire-and-forget, matching the trait contract.
struct LocalTokioSpawner;

impl LocalSpawner for LocalTokioSpawner {
    fn spawn_local(&self, fut: impl core::future::Future<Output = ()> + 'static) {
        drop(tokio::task::spawn_local(fut));
    }
}

/// `Timer` backed by `tokio::time::sleep`. The boxed-future shape
/// matches `tests/bare_metal_e2e.rs`'s `MockTimer` so the harness
/// reads consistently with the parent crate's reference test.
#[derive(Clone)]
struct LocalTimer;

impl Timer for LocalTimer {
    type SleepFuture<'a> = Pin<Box<dyn core::future::Future<Output = ()> + 'a>>;

    fn sleep(&self, duration: Duration) -> Self::SleepFuture<'_> {
        Box::pin(async move {
            tokio::time::sleep(duration).await;
        })
    }
}

type SubKey = (u16, u16, u16, SocketAddrV4);

#[derive(Clone, Default)]
struct MockSubscriptions(Arc<std::sync::Mutex<Vec<SubKey>>>);

impl SubscriptionHandle for MockSubscriptions {
    // Boxed `!Send` futures — the `spawn_local` paths that exercise
    // this loopback don't need `Send` and the `Mutex` is only used
    // synchronously inside.
    type SubscribeFuture<'a> =
        core::pin::Pin<Box<dyn core::future::Future<Output = Result<(), SubscribeError>> + 'a>>;
    type UnsubscribeFuture<'a> = core::pin::Pin<Box<dyn core::future::Future<Output = ()> + 'a>>;

    fn subscribe(
        &self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
        subscriber_addr: SocketAddrV4,
    ) -> Self::SubscribeFuture<'_> {
        let this = self.0.clone();
        Box::pin(async move {
            let mut guard = this.lock().unwrap();
            let key = (service_id, instance_id, event_group_id, subscriber_addr);
            if !guard.contains(&key) {
                guard.push(key);
            }
            Ok(())
        })
    }

    fn unsubscribe(
        &self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
        subscriber_addr: SocketAddrV4,
    ) -> Self::UnsubscribeFuture<'_> {
        let this = self.0.clone();
        Box::pin(async move {
            let mut guard = this.lock().unwrap();
            guard.retain(|e| *e != (service_id, instance_id, event_group_id, subscriber_addr));
        })
    }

    fn for_each_subscriber<'a, F>(
        &'a self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
        mut f: F,
    ) -> impl core::future::Future<Output = usize> + 'a
    where
        F: FnMut(&Subscriber) + 'a,
    {
        let this = self.0.clone();
        async move {
            let guard = this.lock().unwrap();
            let mut count = 0;
            for (s, i, e, addr) in guard.iter() {
                if *s == service_id && *i == instance_id && *e == event_group_id {
                    let sub = Subscriber::new(*addr, *s, *i, *e);
                    f(&sub);
                    count += 1;
                }
            }
            count
        }
    }
}

// `Poll` is imported above for `LocalSpawner` impls; flag it as
// in-use so a `cargo clippy --tests -D warnings` build doesn't
// trip on the otherwise-unused import. (`Poll` is brought in
// because it's the canonical paired import alongside `Pin` for
// hand-rolled futures, even though `LoopbackTestChannels`'
// generated code uses the higher-level macro shape.)
#[allow(dead_code)]
fn _poll_use(p: Poll<()>) -> Poll<()> {
    p
}

// ── SOME/IP Client+Server tests ─────────────────────────────────────

/// Two embassy-net stacks bridged by the loopback driver pair, with
/// a `simple_someip::Server` on stack A announcing `OfferService`
/// via `announcement_loop_local` and a `simple_someip::Client` on
/// stack B receiving the SD broadcast via `bind_discovery`.
///
/// Asserts: the SD `OfferService` propagates through the embassy-net
/// stacks and surfaces on the Client's update stream within 5 s.
#[tokio::test(flavor = "current_thread")]
async fn client_receives_server_sd_announcement() {
    let (drv_a, drv_b) = LoopbackDriver::pair();
    let stack_a = build_stack(drv_a, IP_A, SEED_A);
    let stack_b = build_stack(drv_b, IP_B, SEED_B);

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            tokio::task::spawn_local(async move { stack_a.run().await });
            tokio::task::spawn_local(async move { stack_b.run().await });

            // Both stacks join the SD multicast group at the
            // smoltcp level. The `EmbassyNetFactory`'s adapter
            // `join_multicast_v4` is a documented no-op (per the
            // factory.rs docstring) — multicast subscription has
            // to happen on the `Stack` directly, before any
            // `Server` / `Client` constructs sockets that need it.
            // embassy-net's `Stack::join_multicast_group` takes
            // `T: Into<IpAddress>`. There is no
            // `core::net::Ipv4Addr -> IpAddress` blanket impl in
            // embassy-net 0.4, so explicitly construct the
            // smoltcp-flavour `Ipv4Address` from octets.
            let sd_mc =
                embassy_net::Ipv4Address(simple_someip::protocol::sd::MULTICAST_IP.octets());
            stack_a
                .join_multicast_group(sd_mc)
                .await
                .expect("stack A multicast join");
            stack_b
                .join_multicast_group(sd_mc)
                .await
                .expect("stack B multicast join");

            // ── Server on stack A ────────────────────────────────
            let server_pool: &'static SocketPool<8, LINK_MTU, LINK_MTU> =
                Box::leak(Box::new(SocketPool::new()));
            let server_factory = EmbassyNetFactory::new(stack_a, server_pool);
            let server_e2e: Arc<std::sync::Mutex<E2ERegistry>> =
                Arc::new(std::sync::Mutex::new(E2ERegistry::new()));
            let server_subs = MockSubscriptions::default();
            // Service id 0x5BAA (just a witness) at port 30500 on
            // stack A's interface IP.
            let server_config = ServerConfig::new(0x5BAA, 1)
                .with_interface(IP_A)
                .with_local_port(30500);

            let server_deps = ServerDeps {
                factory: server_factory,
                timer: LocalTimer,
                e2e_registry: server_e2e,
                subscriptions: server_subs,
                non_sd_observer: None,
            };

            // Default `H = Arc<F::Socket>`. `Arc<T>:
            // WrappableSocketHandle` works for any `T: TransportSocket
            // + 'static`, so `Arc<EmbassyNetSocket>` (which is
            // `!Sync`) compiles here. The annotation is explicit so
            // type inference doesn't have to chase `H` across the
            // deps-bundle indirection.
            let (server, _handles, _run): (
                Server<_, _, _, _, Arc<simple_someip_embassy_net::EmbassyNetSocket>>,
                _,
                _,
            ) = Server::new_with_deps(server_deps, server_config, false)
                .await
                .expect("server construction over embassy-net");

            // Receive + announce share the combined run-future. The
            // constructor's `_run` is the alloc-backed version; we
            // use `run_with_buffers` here because
            // `EmbassyNetSocket: !Sync` makes the `_run` future
            // `!Send` and we want explicit static buffers anyway.
            tokio::task::spawn_local(server.run_with_buffers(
                Box::leak(Box::new([0u8; 65535])),
                Box::leak(Box::new([0u8; 65535])),
                Box::leak(Box::new([0u8; simple_someip::UDP_BUFFER_SIZE])),
                Box::leak(Box::new([0u8; simple_someip::UDP_BUFFER_SIZE])),
            ));

            // ── Client on stack B ────────────────────────────────
            let client_pool: &'static SocketPool<8, LINK_MTU, LINK_MTU> =
                Box::leak(Box::new(SocketPool::new()));
            let client_factory = EmbassyNetFactory::new(stack_b, client_pool);
            let client_e2e: Arc<std::sync::Mutex<E2ERegistry>> =
                Arc::new(std::sync::Mutex::new(E2ERegistry::new()));
            let client_iface: Arc<RwLock<Ipv4Addr>> = Arc::new(RwLock::new(IP_B));

            let buf_pool: &'static BufferPool<2, LINK_MTU> =
                Box::leak(Box::new(BufferPool::new()));
            let client_deps = ClientDeps {
                factory: client_factory,
                spawner: LocalTokioSpawner,
                timer: LocalTimer,
                e2e_registry: client_e2e,
                interface: client_iface,
                buffer_provider: StaticBufferProvider(buf_pool),
            };

            let (client, mut updates, run_fut) =
                Client::<
                    RawPayload,
                    Arc<std::sync::Mutex<E2ERegistry>>,
                    Arc<RwLock<Ipv4Addr>>,
                    LoopbackTestChannels,
                >::new_with_deps_local(client_deps, false);
            tokio::task::spawn_local(run_fut);

            client.bind_discovery().await.expect("bind_discovery");

            // ── Wait for SD announcement to land ─────────────────
            let received = tokio::time::timeout(Duration::from_secs(5), async {
                while let Some(update) = updates.recv().await {
                    if matches!(update, ClientUpdate::DiscoveryUpdated(_)) {
                        return true;
                    }
                }
                false
            })
            .await;

            assert!(
                received.unwrap_or(false),
                "client did not see server's SD OfferService via embassy-net loopback within 5s",
            );
        })
        .await;
}

/// Passive-server variant: the server doesn't emit SD announcements
/// (matching the parent crate's `client_send_request_server_runloop_stable`
/// pattern). The client uses `add_endpoint` + `send_to_service` to
/// drive a SOME/IP request through the embassy-net loopback toward
/// the server's unicast port. We assert the client's serialize +
/// transmit path completes (`send_to_service` returns Ok) — NOT
/// that the server's run loop processes the bytes, because the
/// passive server's `run()` returns `Err(InvalidUsage)` immediately
/// (passive servers expect SD to be driven externally) and is
/// therefore not actually running. A response isn't asserted because
/// `simple_someip::Server` has no public request-handler API.
///
/// In short: this is a TX-side smoke test for the embassy-net
/// adapter's send path, not a server-runloop test. Despite the
/// historical name (kept for git-blame continuity with the parent
/// reference test).
#[tokio::test(flavor = "current_thread")]
async fn client_send_request_server_runloop_stable() {
    let (drv_a, drv_b) = LoopbackDriver::pair();
    let stack_a = build_stack(drv_a, IP_A, SEED_A);
    let stack_b = build_stack(drv_b, IP_B, SEED_B);

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            tokio::task::spawn_local(async move { stack_a.run().await });
            tokio::task::spawn_local(async move { stack_b.run().await });

            // No multicast join here — passive server doesn't use SD,
            // and the client doesn't need discovery (we'll wire it up
            // via add_endpoint instead).

            // ── Server on stack A (passive) ──────────────────────
            let server_pool: &'static SocketPool<8, LINK_MTU, LINK_MTU> =
                Box::leak(Box::new(SocketPool::new()));
            let server_factory = EmbassyNetFactory::new(stack_a, server_pool);
            let server_e2e: Arc<std::sync::Mutex<E2ERegistry>> =
                Arc::new(std::sync::Mutex::new(E2ERegistry::new()));
            let server_subs = MockSubscriptions::default();
            let service_id = 0x5BBB_u16;
            let instance_id = 1_u16;
            let server_port = 30600_u16;
            let server_config = ServerConfig::new(service_id, instance_id)
                .with_interface(IP_A)
                .with_local_port(server_port);

            let server_deps = ServerDeps {
                factory: server_factory,
                timer: LocalTimer,
                e2e_registry: server_e2e,
                subscriptions: server_subs,
                non_sd_observer: None,
            };

            // Explicit `Arc<EmbassyNetSocket>` `H` so the compiler
            // doesn't have to invent it across the deps-bundle
            // indirection. Same shape as the equivalent annotation
            // in `simple_someip`'s SD-NACK test.
            let (server, _handles, _run): (
                Server<_, _, _, _, Arc<simple_someip_embassy_net::EmbassyNetSocket>>,
                _,
                _,
            ) = Server::new_passive_with_deps(server_deps, server_config)
                .await
                .expect("passive server construction");

            // NOTE: we do NOT spawn `server.run()` here. A passive
            // server's `run()` returns `Err(InvalidUsage)`
            // immediately (passive servers expect SD to be driven
            // externally), so the spawn would just be a no-op task
            // exiting on first poll. The server is constructed only
            // so its unicast socket bind happens — the kernel-level
            // recv buffer absorbs the client's request bytes
            // independently of any application run-loop.
            let _ = &server; // anchor binding so the unicast bind sticks

            // ── Client on stack B ────────────────────────────────
            let client_pool: &'static SocketPool<8, LINK_MTU, LINK_MTU> =
                Box::leak(Box::new(SocketPool::new()));
            let client_factory = EmbassyNetFactory::new(stack_b, client_pool);
            let client_e2e: Arc<std::sync::Mutex<E2ERegistry>> =
                Arc::new(std::sync::Mutex::new(E2ERegistry::new()));
            let client_iface: Arc<RwLock<Ipv4Addr>> = Arc::new(RwLock::new(IP_B));

            let buf_pool: &'static BufferPool<8, LINK_MTU> =
                Box::leak(Box::new(BufferPool::new()));
            let client_deps = ClientDeps {
                factory: client_factory,
                spawner: LocalTokioSpawner,
                timer: LocalTimer,
                e2e_registry: client_e2e,
                interface: client_iface,
                buffer_provider: StaticBufferProvider(buf_pool),
            };

            let (client, _updates, run_fut) = Client::<
                RawPayload,
                Arc<std::sync::Mutex<E2ERegistry>>,
                Arc<RwLock<Ipv4Addr>>,
                LoopbackTestChannels,
            >::new_with_deps_local(client_deps, false);
            tokio::task::spawn_local(run_fut);

            // Register the server's unicast endpoint. The 0 in the
            // 4th slot is the eventgroup id (unused for a plain
            // request-response add_endpoint).
            let server_addr = SocketAddrV4::new(IP_A, server_port);
            client
                .add_endpoint(service_id, instance_id, server_addr, 0)
                .await
                .expect("add_endpoint");

            // Build + send a SOME/IP request. The wire payload is
            // arbitrary — what we're proving is the request fully
            // serializes, hits the wire via embassy-net, and the
            // server's `recv_from` loop accepts it without panicking.
            let msg_id = MessageId::new_from_service_and_method(service_id, 0x0001);
            let payload_bytes = [0xDE_u8, 0xAD, 0xBE, 0xEF];
            let payload = RawPayload::from_payload_bytes(msg_id, &payload_bytes)
                .expect("RawPayload::from_payload_bytes");
            let request = Message::<RawPayload>::new(
                SomeIpHeader::new(
                    msg_id,
                    0x0001_0001, // request_id: client_id << 16 | session_id
                    1,           // protocol_version
                    1,           // interface_version
                    MessageTypeField::new(MessageType::Request, false),
                    ReturnCode::Ok,
                    payload_bytes.len(),
                ),
                payload,
            );

            let _pending = client
                .send_to_service(service_id, instance_id, request)
                .await
                .expect("send_to_service over embassy-net");

            // Give the server time to process before the test
            // tears down. Without a registered handler we can't
            // assert a response — same caveat as the parent
            // reference test.
            tokio::time::sleep(Duration::from_millis(200)).await;

            // Test passes if everything above ran without panic and
            // `add_endpoint` + `send_to_service` returned Ok.
        })
        .await;
}
