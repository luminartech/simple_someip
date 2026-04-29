//! Phase 19e — Adapter-level loopback test.
//!
//! Two `embassy_net::Stack` instances bridged by an in-memory
//! `LoopbackDriver` pair (no kernel TUN device, no privileges
//! required). Validates the `simple-someip-embassy-net` adapter
//! (Phases 19a–c) against a real `embassy_net::Stack`:
//!
//! * **`adapter_udp_roundtrip`** — bind two `EmbassyNetSocket`s,
//!   one per stack, send a UDP datagram from A to B, assert
//!   byte-equality + source-address. Tightest test of `bind` /
//!   `send_to` / `recv_from` / `local_addr` end-to-end.
//!
//! SOME/IP-level Client+Server integration is **not** in this
//! phase — it lands in 19g. Reason: `Server` requires
//! `F::Socket: Send + Sync` on every impl block (`mod.rs:275`,
//! `:430`, `:1065`), but `embassy_net::udp::UdpSocket<'static>`
//! is `!Sync` because it borrows from `Stack`'s
//! `RefCell<Inner<D>>`. Phase 19f adds the parallel `_local`
//! constructor + impl block on `Server` to mirror Client's
//! `new_with_deps_local`; once that ships, 19g lifts the
//! `tests/bare_metal_e2e.rs` harness onto these stacks. See
//! `bare_metal_plan_v3.md` for the rest.
//!
//! Runtime: `#[tokio::test(flavor = "current_thread")]` plus a
//! `LocalSet` driving the per-stack `spawn_local` runners.
//! `Stack<LoopbackDriver>` is `!Sync` (RefCell internals), so
//! `Stack::run()` is `!Send` — multi-threaded `tokio::spawn`
//! does not type-check.

use core::net::{Ipv4Addr, SocketAddrV4};
use core::task::{Context, Waker};
use core::time::Duration;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use embassy_net::driver::{Capabilities, Driver, HardwareAddress, LinkState, RxToken, TxToken};
use embassy_net::{Config, Stack, StackResources, StaticConfigV4};

use simple_someip::transport::{SocketOptions, TransportFactory, TransportSocket};
use simple_someip_embassy_net::{EmbassyNetFactory, SocketPool};

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
        // 1500 matches simple-someip's `UDP_BUFFER_SIZE`. The
        // `medium-ip` smoltcp feature lets us skip the
        // Ethernet-frame layer and ship raw IP packets, which is
        // what `HardwareAddress::Ip` below also requests.
        caps.max_transmission_unit = 1500;
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
// integration to use `Client::new_with_deps_local` (matching the
// `LocalSpawner` trait shipped in phase 17 specifically for
// !Send-bound transports).

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

            let pool_a: &'static SocketPool<2, 1500, 1500> = Box::leak(Box::new(SocketPool::new()));
            let pool_b: &'static SocketPool<2, 1500, 1500> = Box::leak(Box::new(SocketPool::new()));
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

// SOME/IP Client+Server wiring deferred — see phase 19f scoping
// added to `bare_metal_plan_v3.md` 2026-04-29. Server's storage of
// `Arc<F::Socket>` propagates `Send + Sync` through every impl
// block, and embassy-net's `UdpSocket<'static>` is `!Sync` (and
// likely `!Send`) because it borrows from the `Stack`'s
// `RefCell<Inner>`. Adding `_local` constructors alone is
// insufficient; the storage shape needs to be abstracted (handle
// trait similar to `InterfaceHandle` / `SubscriptionHandle`) before
// the SOME/IP-level integration test can wire `Server` through this
// adapter. Phase 19e ships with the adapter-level UDP roundtrip
// above as the verifiable assertion that 19a-c work end-to-end.
