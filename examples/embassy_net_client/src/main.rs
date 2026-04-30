//! Host-runnable demonstration of `simple-someip` over the
//! `simple-someip-embassy-net` adapter.
//!
//! # What this example shows
//!
//! Two `embassy_net::Stack` instances bridged by an in-memory
//! `LoopbackDriver` pair (no kernel TUN, no privileges). A real
//! `simple_someip::Server` on stack A emits SD `OfferService`
//! announcements via [`Server::announcement_loop_local`]; a real
//! `simple_someip::Client` on stack B binds discovery via the
//! adapter's `EmbassyNetFactory` and prints each SD message it
//! receives.
//!
//! The example demonstrates the wiring patterns a firmware author
//! needs to reproduce:
//!
//! | Pattern | This example | Firmware replacement |
//! |---|---|---|
//! | Executor | `tokio::main` (`current_thread` + `LocalSet`) | `#[embassy_executor::main]` |
//! | Driver | `LoopbackDriver` (in-memory pipe pair) | hardware MAC driver (lan8742, w5500, vendor IP) |
//! | `SocketPool` | `static`-leaked at startup | `static` declaration in firmware boot, no leak |
//! | `Timer` | `tokio::time::sleep` | `embassy_time::Timer::after` |
//! | `LocalSpawner` | `tokio::task::spawn_local` | `embassy_executor::Spawner::spawn` |
//! | `SocketHandle` `H` | `Arc<EmbassyNetSocket>` (alloc) | same on alloc-targets, `StaticSocketHandle` on no-alloc |
//!
//! Build + run:
//!
//! ```text
//! cargo run -p embassy_net_client
//! ```
//!
//! Expected output (truncated):
//!
//! ```text
//! [server] announcement loop spawned, emitting OfferService(0x5BAA) every 1s
//! [client] discovery bound on 169.254.1.2:30490
//! [client] received SD update: DiscoveryUpdated { ... }
//! [example] roundtrip complete; exiting
//! ```
//!
//! The example exits cleanly after the first SD message reaches the
//! Client.

use core::future::Future;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::pin::Pin;
use core::task::{Context, Waker};
use core::time::Duration;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, RwLock};

use embassy_net::driver::{Capabilities, Driver, HardwareAddress, LinkState, RxToken, TxToken};
use embassy_net::{Config, Stack, StackResources, StaticConfigV4};

use simple_someip::client::Error as ClientError;
use simple_someip::client::{ClientUpdate, ControlMessage, ReceivedMessage, SendMessage};
use simple_someip::define_static_channels;
use simple_someip::e2e::E2ERegistry;
use simple_someip::protocol::sd::RebootFlag;
use simple_someip::server::{ServerConfig, SubscribeError, Subscriber, SubscriptionHandle};
use simple_someip::transport::{LocalSpawner, Timer};
use simple_someip::{Client, ClientDeps, RawPayload, Server, ServerDeps};
use simple_someip_embassy_net::{EmbassyNetFactory, EmbassyNetSocket, LINK_MTU, SocketPool};

// ── LoopbackDriver pair ──────────────────────────────────────────────
//
// Same shape as `simple-someip-embassy-net/tests/loopback.rs`: each
// `Pipe` is a one-direction queue + waker; the pair of drivers
// shares two pipes (A→B and B→A) so smoltcp on each side exchanges
// raw IP frames in memory. Real firmware replaces `LoopbackDriver`
// with a hardware MAC driver implementing the same `Driver` trait.

#[derive(Default)]
struct Pipe {
    queue: Mutex<VecDeque<Vec<u8>>>,
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
        match slot.as_ref() {
            Some(existing) if existing.will_wake(w) => {}
            _ => *slot = Some(w.clone()),
        }
    }
}

struct LoopbackDriver {
    rx: Arc<Pipe>,
    tx: Arc<Pipe>,
}

impl LoopbackDriver {
    fn pair() -> (Self, Self) {
        let a_to_b = Arc::new(Pipe::default());
        let b_to_a = Arc::new(Pipe::default());
        (
            LoopbackDriver {
                rx: Arc::clone(&b_to_a),
                tx: Arc::clone(&a_to_b),
            },
            LoopbackDriver {
                rx: a_to_b,
                tx: b_to_a,
            },
        )
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
        Some(LoopbackTxToken {
            tx: Arc::clone(&self.tx),
        })
    }

    fn link_state(&mut self, _cx: &mut Context) -> LinkState {
        LinkState::Up
    }

    fn capabilities(&self) -> Capabilities {
        let mut caps = Capabilities::default();
        caps.max_transmission_unit = LINK_MTU;
        caps.max_burst_size = None;
        caps
    }

    fn hardware_address(&self) -> HardwareAddress {
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

const STACK_SOCKETS: usize = 8;
const IP_A: Ipv4Addr = Ipv4Addr::new(169, 254, 1, 1);
const IP_B: Ipv4Addr = Ipv4Addr::new(169, 254, 1, 2);
const SEED_A: u64 = 0x1111_2222_3333_4444;
const SEED_B: u64 = 0x5555_6666_7777_8888;

fn build_stack(driver: LoopbackDriver, ip: Ipv4Addr, seed: u64) -> &'static Stack<LoopbackDriver> {
    let resources: &'static mut StackResources<STACK_SOCKETS> =
        Box::leak(Box::new(StackResources::<STACK_SOCKETS>::new()));
    let config = Config::ipv4_static(StaticConfigV4 {
        address: embassy_net::Ipv4Cidr::new(embassy_net::Ipv4Address(ip.octets()), 24),
        gateway: None,
        // `Default::default()` picks up embassy-net's bundled
        // `heapless::Vec` (re-exported privately) rather than this
        // crate's heapless dep — different majors don't share types,
        // and we don't want a direct heapless dep here just to spell
        // out the type. `#[allow]` for clippy::default_trait_access:
        // the inference is exactly the point.
        #[allow(clippy::default_trait_access)]
        dns_servers: Default::default(),
    });
    Box::leak(Box::new(Stack::new(driver, config, resources, seed)))
}

// ── Static channels for the Client ──────────────────────────────────

define_static_channels! {
    name: ExampleChannels,
    oneshot: [
        (Result<(), ClientError>, 8),
        (Result<RawPayload, ClientError>, 4),
        (Result<RebootFlag, ClientError>, 4),
    ],
    bounded: [
        ((ControlMessage<RawPayload, ExampleChannels>, 4), 2),
        ((SendMessage<RawPayload, ExampleChannels>, 16), 4),
        ((Result<ReceivedMessage<RawPayload>, ClientError>, 16), 4),
    ],
    unbounded: [
        (ClientUpdate<RawPayload>, 2),
    ],
}

// ── Spawner / Timer / Subscriptions ─────────────────────────────────

struct LocalTokioSpawner;

impl LocalSpawner for LocalTokioSpawner {
    fn spawn_local(&self, fut: impl Future<Output = ()> + 'static) {
        drop(tokio::task::spawn_local(fut));
    }
}

#[derive(Clone)]
struct LocalTimer;

impl Timer for LocalTimer {
    type SleepFuture<'a> = Pin<Box<dyn Future<Output = ()> + 'a>>;

    fn sleep(&self, duration: Duration) -> Self::SleepFuture<'_> {
        Box::pin(async move {
            tokio::time::sleep(duration).await;
        })
    }
}

type SubKey = (u16, u16, u16, SocketAddrV4);

#[derive(Clone, Default)]
struct InMemorySubscriptions(Arc<Mutex<Vec<SubKey>>>);

impl SubscriptionHandle for InMemorySubscriptions {
    fn subscribe(
        &self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
        subscriber_addr: SocketAddrV4,
    ) -> impl Future<Output = Result<(), SubscribeError>> + '_ {
        let this = self.0.clone();
        async move {
            let mut g = this.lock().unwrap();
            let k = (service_id, instance_id, event_group_id, subscriber_addr);
            if !g.contains(&k) {
                g.push(k);
            }
            Ok(())
        }
    }

    fn unsubscribe(
        &self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
        subscriber_addr: SocketAddrV4,
    ) -> impl Future<Output = ()> + '_ {
        let this = self.0.clone();
        async move {
            let mut g = this.lock().unwrap();
            g.retain(|e| *e != (service_id, instance_id, event_group_id, subscriber_addr));
        }
    }

    fn for_each_subscriber<'a, F>(
        &'a self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
        mut f: F,
    ) -> impl Future<Output = usize> + 'a
    where
        F: FnMut(&Subscriber) + 'a,
    {
        let this = self.0.clone();
        async move {
            let g = this.lock().unwrap();
            let mut n = 0;
            for (s, i, e, addr) in g.iter() {
                if *s == service_id && *i == instance_id && *e == event_group_id {
                    f(&Subscriber::new(*addr, *s, *i, *e));
                    n += 1;
                }
            }
            n
        }
    }
}

// ── main ─────────────────────────────────────────────────────────────

const SERVICE_ID: u16 = 0x5BAA;
const INSTANCE_ID: u16 = 1;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let (drv_a, drv_b) = LoopbackDriver::pair();
    let stack_a = build_stack(drv_a, IP_A, SEED_A);
    let stack_b = build_stack(drv_b, IP_B, SEED_B);

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            tokio::task::spawn_local(async move { stack_a.run().await });
            tokio::task::spawn_local(async move { stack_b.run().await });

            // Multicast group join lives on `Stack`, not on the
            // socket — the adapter's `join_multicast_v4` is a
            // documented no-op. Both sides need to be members
            // for SD multicast to flow.
            let sd_mc =
                embassy_net::Ipv4Address(simple_someip::protocol::sd::MULTICAST_IP.octets());
            stack_a
                .join_multicast_group(sd_mc)
                .await
                .expect("server stack joined SD multicast");
            stack_b
                .join_multicast_group(sd_mc)
                .await
                .expect("client stack joined SD multicast");

            // ── Server on stack A ────────────────────────────────
            let server_pool: &'static SocketPool<8, LINK_MTU, LINK_MTU> =
                Box::leak(Box::new(SocketPool::new()));
            let server_factory = EmbassyNetFactory::new(stack_a, server_pool);
            let server_e2e: Arc<Mutex<E2ERegistry>> = Arc::new(Mutex::new(E2ERegistry::new()));
            let server_config = ServerConfig::new(IP_A, 30500, SERVICE_ID, INSTANCE_ID);

            let server_deps = ServerDeps {
                factory: server_factory,
                timer: LocalTimer,
                e2e_registry: server_e2e,
                subscriptions: InMemorySubscriptions::default(),
            };

            // Phase 19f: default `H = Arc<F::Socket>`. Annotation
            // is explicit because type inference can't chase H
            // across the `ServerDeps` indirection.
            let server: Server<_, _, _, _, Arc<EmbassyNetSocket>> =
                Server::new_with_deps(server_deps, server_config, false)
                    .await
                    .expect("server construction over embassy-net");

            // `_local` because `EmbassyNetSocket: !Sync` (it borrows
            // from `Stack<LoopbackDriver>`'s `RefCell`-bearing
            // internals); the Send-bounded `announcement_loop`
            // doesn't typecheck for our `H`.
            let announce_fut = server
                .announcement_loop_local()
                .expect("announcement_loop_local");
            tokio::task::spawn_local(announce_fut);
            println!(
                "[server] announcement loop spawned, emitting OfferService(0x{SERVICE_ID:04X}) every 1s"
            );

            // ── Client on stack B ────────────────────────────────
            let client_pool: &'static SocketPool<8, LINK_MTU, LINK_MTU> =
                Box::leak(Box::new(SocketPool::new()));
            let client_factory = EmbassyNetFactory::new(stack_b, client_pool);
            let client_e2e: Arc<Mutex<E2ERegistry>> = Arc::new(Mutex::new(E2ERegistry::new()));
            let client_iface: Arc<RwLock<Ipv4Addr>> = Arc::new(RwLock::new(IP_B));

            let client_deps = ClientDeps {
                factory: client_factory,
                spawner: LocalTokioSpawner,
                timer: LocalTimer,
                e2e_registry: client_e2e,
                interface: client_iface,
            };

            let (client, mut updates, run_fut) = Client::<
                RawPayload,
                Arc<Mutex<E2ERegistry>>,
                Arc<RwLock<Ipv4Addr>>,
                ExampleChannels,
            >::new_with_deps_local(
                client_deps, false
            );
            tokio::task::spawn_local(run_fut);

            client
                .bind_discovery()
                .await
                .expect("client bound discovery");
            println!("[client] discovery bound on {IP_B}:30490");

            // ── Wait for the SD announcement ─────────────────────
            let result = tokio::time::timeout(Duration::from_secs(5), async {
                while let Some(update) = updates.recv().await {
                    println!("[client] received SD update: {update:?}");
                    if matches!(update, ClientUpdate::DiscoveryUpdated(_)) {
                        return true;
                    }
                }
                false
            })
            .await;

            match result {
                Ok(true) => println!("[example] roundtrip complete; exiting"),
                Ok(false) => println!("[example] update stream closed before SD arrived"),
                Err(_) => println!("[example] TIMEOUT — no SD message in 5s"),
            }
        })
        .await;
}
