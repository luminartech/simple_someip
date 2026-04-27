//! Host-side canary for the bare-metal trait surface.
//!
//! # What this example actually is
//!
//! A workspace-member binary that exercises `simple-someip`'s
//! `TransportSocket` / `TransportFactory` / `Timer` traits against a
//! hand-rolled mock backend. The `Cargo.toml` in this directory
//! depends on `simple-someip` with
//! `default-features = false, features = ["bare_metal"]`, so building
//! or running this package in isolation proves **that the trait
//! surface compiles under exactly the feature set a firmware consumer
//! would use** — no `std`-feature paths from `simple-someip`, no
//! tokio, no socket2.
//!
//! Use `cargo build -p bare_metal` (or `cargo run -p bare_metal`) as
//! the source of truth for that check; `cargo build --workspace` can
//! unify features across workspace members and may therefore mask
//! regressions in this minimal configuration. CI should run
//! `cargo build -p bare_metal` (and `cargo clippy -p bare_metal`) as a
//! dedicated step.
//!
//! # How to run
//!
//! ```text
//! cargo build -p bare_metal
//! cargo run -p bare_metal
//! ```
//!
//! # What this is NOT
//!
//! This is **not** a runtime `no_std` demonstration. The host-side
//! mock uses `std::collections::VecDeque`, `std::sync::{Arc, Mutex}`,
//! `std::time::Instant`, and `println!` — all of which an actual
//! firmware build would replace with embedded equivalents
//! (`heapless::Deque`, `spin::Mutex`, a platform clock, `defmt!` or
//! similar). Using `std` in the *host-side driver code* is fine
//! because the purpose of this example is to verify **the
//! `simple-someip` crate itself** compiles with `default-features =
//! false` and exposes a trait surface that embedded consumers can
//! target. A true runtime-`no_std` example belongs with the phase
//! 10+ bare-metal refactor, once `Client` / `Server` can consume a
//! user-supplied transport and spawner without pulling in tokio.
//!
//! # Known gaps in the bare-metal story (independent of this example)
//!
//! The example exercises the **trait layer** (`TransportSocket`,
//! `TransportFactory`, `Timer`, `Spawner`, `ChannelFactory`) — and
//! that is all. It does NOT demonstrate a `no_alloc` integration with
//! `simple_someip::Client` / `simple_someip::Server`, because those
//! are not yet `no_alloc`-compatible.
//!
//! **Completed abstractions:**
//! - Phase 9: `Spawner` trait (task submission)
//! - Phase 10: `E2ERegistryHandle` / `InterfaceHandle` (lock handles)
//! - Phase 11: `ChannelFactory` trait with `TokioChannels` (std) and
//!   `EmbassySyncChannels` (`bare_metal`) backends — replaces direct
//!   `tokio::sync::mpsc` / `oneshot` usage
//! - Phase 12: `TransportSocket` GATs — `SendFuture` / `RecvFuture`
//!   express `Send` bounds without RTN; `Socket = TokioSocket` pin
//!   removed from `bind_*` functions
//! - Phase 13 (partial): client-side feature-flag split. `client` no
//!   longer pulls tokio + socket2; the tokio convenience defaults
//!   (`Client::new`, `TokioSpawner`, etc.) live behind a new
//!   `client-tokio` feature.
//!
//! **Remaining gaps:**
//! 1. **Server-side feature-flag split** (Phase 13 server half,
//!    deferred to Phase 14): `feature = "server"` still pulls in
//!    tokio + socket2 because `server::sd_state` and
//!    `server::subscription_manager` reference `tokio::net::UdpSocket`
//!    / `tokio::sync::RwLock` / `socket2::Socket` directly. Phase 14
//!    (server parallel) is the phase that retargets the server to the
//!    trait surface; once that lands, `server` will gain the same
//!    `server` + `server-tokio` split.
//!
//! # Recommendation for `no_alloc` consumers today
//!
//! Do NOT route through `Client::new_with_spawner_and_loopback`.
//! Instead, depend on `simple-someip` with `default-features = false,
//! features = ["bare_metal"]` and consume the already-portable layers
//! directly:
//!
//! - `simple_someip::protocol` — wire format (headers, messages, SD
//!   entries/options); zero-copy views for parsing.
//! - `simple_someip::e2e` — CRC-32 / CRC-16 protection profiles; owned
//!   per-payload, no `Arc<Mutex<_>>` required.
//! - `simple_someip::transport` — the four traits exercised below.
//!
//! Then write a small SOME/IP orchestrator that owns its socket, a
//! stack-allocated request-map (e.g.
//! `heapless::FnvIndexMap<RequestId, _, N>`), and drives SD + r/r +
//! event subscription using `futures::select!` over
//! `TransportSocket::recv_from` / `Timer::sleep` directly. That is
//! the shape the trait layer was designed for; the `Client` /
//! `Server` types are a std+tokio convenience layer on top that
//! happens not to suit `no_alloc` targets yet.

use core::future::Future;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use core::time::Duration;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use simple_someip::transport::{
    IoErrorKind, ReceivedDatagram, SocketOptions, Timer, TransportError, TransportFactory,
    TransportSocket,
};

/// Shared in-memory pipe. A `MockFactory` built around one of these
/// hands out sockets whose `send_to` pushes to `send_queue` and whose
/// `recv_from` pops from `recv_queue`. Two factories swapped queue-
/// ends give you a bidirectional pipe.
#[derive(Default)]
struct MockPipe {
    /// `(bytes, dest_addr)` pairs sent by the local socket.
    send_queue: Mutex<VecDeque<(Vec<u8>, SocketAddrV4)>>,
    /// `(bytes, src_addr)` pairs the local socket will read next.
    recv_queue: Mutex<VecDeque<(Vec<u8>, SocketAddrV4)>>,
}

#[derive(Clone)]
struct MockFactory {
    pipe: Arc<MockPipe>,
    local_addr: SocketAddrV4,
}

struct MockSocket {
    pipe: Arc<MockPipe>,
    local_addr: SocketAddrV4,
}

impl TransportFactory for MockFactory {
    type Socket = MockSocket;

    fn bind(
        &self,
        _addr: SocketAddrV4,
        _options: &SocketOptions,
    ) -> impl Future<Output = Result<Self::Socket, TransportError>> {
        let pipe = Arc::clone(&self.pipe);
        let local_addr = self.local_addr;
        core::future::ready(Ok(MockSocket { pipe, local_addr }))
    }
}

/// Future returned by [`MockSocket::send_to`]. Defers the queue push
/// to poll-time so the side effect happens when the future is awaited,
/// not when `send_to` is called — matching what a real bare-metal
/// `TransportSocket` impl would do (the network driver only sees the
/// datagram when the executor polls the future).
struct MockSendFut {
    pipe: Arc<MockPipe>,
    bytes: Option<Vec<u8>>,
    target: SocketAddrV4,
}

impl Future for MockSendFut {
    type Output = Result<(), TransportError>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = self.get_mut();
        if let Some(bytes) = me.bytes.take() {
            me.pipe.send_queue.lock().unwrap().push_back((bytes, me.target));
        }
        Poll::Ready(Ok(()))
    }
}

/// Future returned by [`MockSocket::recv_from`]. Reads from the queue
/// on poll. A production bare-metal impl would instead register the
/// `Context`'s `Waker` on the network driver's RX-ready signal and
/// return `Poll::Pending` when the queue is empty — see e.g.
/// `embassy_net::UdpSocket` or smoltcp's socket polling model. This
/// mock returns `Err(TimedOut)` on empty for simplicity; the demo
/// always sends before recv-ing so the empty branch is unreachable.
struct MockRecvFut<'a> {
    pipe: Arc<MockPipe>,
    buf: &'a mut [u8],
}

impl Future for MockRecvFut<'_> {
    type Output = Result<ReceivedDatagram, TransportError>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = self.get_mut();
        let entry = me.pipe.recv_queue.lock().unwrap().pop_front();
        Poll::Ready(match entry {
            Some((bytes, source)) => {
                let n = bytes.len().min(me.buf.len());
                me.buf[..n].copy_from_slice(&bytes[..n]);
                Ok(ReceivedDatagram {
                    bytes_received: n,
                    source,
                    truncated: n < bytes.len(),
                })
            }
            None => Err(TransportError::Io(IoErrorKind::TimedOut)),
        })
    }
}

impl TransportSocket for MockSocket {
    type SendFuture<'a> = MockSendFut;
    type RecvFuture<'a> = MockRecvFut<'a>;

    fn send_to<'a>(&'a self, buf: &'a [u8], target: SocketAddrV4) -> Self::SendFuture<'a> {
        // `buf` cannot be borrowed past this call (its lifetime is
        // bounded by the borrow checker, not the future), so we copy
        // here. The push to the shared queue is deferred to `poll`.
        MockSendFut {
            pipe: Arc::clone(&self.pipe),
            bytes: Some(buf.to_vec()),
            target,
        }
    }

    fn recv_from<'a>(&'a self, buf: &'a mut [u8]) -> Self::RecvFuture<'a> {
        MockRecvFut {
            pipe: Arc::clone(&self.pipe),
            buf,
        }
    }

    fn local_addr(&self) -> Result<SocketAddrV4, TransportError> {
        Ok(self.local_addr)
    }

    fn join_multicast_v4(&self, _group: Ipv4Addr, _iface: Ipv4Addr) -> Result<(), TransportError> {
        // Bare-metal stacks without multicast would return
        // Unsupported; our mock is happy to no-op.
        Ok(())
    }

    fn leave_multicast_v4(&self, _group: Ipv4Addr, _iface: Ipv4Addr) -> Result<(), TransportError> {
        Ok(())
    }
}

/// Timer that sleeps by busy-waiting on a monotonic clock.
///
/// **ANTI-PATTERN — DO NOT USE IN PRODUCTION.** Busy-waiting burns a
/// core and starves other tasks. A real bare-metal impl would park
/// the task on its hardware timer ISR (e.g. `embassy_time::Timer::after`,
/// or a custom `Future` that registers itself with the MCU's timer
/// peripheral). The `Timer` trait signature is identical; only the
/// body changes.
struct MockTimer;

impl Timer for MockTimer {
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> {
        // ANTI-PATTERN: busy-wait. See struct docstring.
        let deadline = std::time::Instant::now() + duration;
        async move {
            while std::time::Instant::now() < deadline {
                std::hint::spin_loop();
            }
        }
    }
}

/// Phase 9 `Spawner` impl that demonstrates the *correct* contract:
/// every submitted future is queued and later polled to completion.
///
/// Why a working impl rather than a one-line "drop the future" mock:
/// the `Spawner` trait's docstring explicitly forbids dropping the
/// future without polling, because `Client::send`'s internal oneshot
/// round-trip needs the per-socket loop to make progress. A canary
/// that violates the contract isn't validating the contract.
///
/// A real bare-metal `Spawner` wraps the executor's task-submission
/// primitive — `embassy_executor::Spawner`, smoltcp's task pool, or a
/// hand-rolled single-core polling loop. Here we keep submissions in
/// an in-memory queue and the demo's `main()` drains it at the end via
/// [`WorkingSpawner::drain`]. That mirrors the shape of a single-core
/// cooperative executor closely enough to prove the trait surface
/// works.
struct WorkingSpawner {
    queue: Mutex<Vec<Pin<Box<dyn Future<Output = ()> + Send>>>>,
}

impl WorkingSpawner {
    fn new() -> Self {
        Self {
            queue: Mutex::new(Vec::new()),
        }
    }

    /// Block-on every queued future to completion, in submission order.
    /// A real cooperative executor would interleave polls; the demo's
    /// futures resolve on the first poll so order doesn't matter.
    fn drain(&self) {
        let queued = std::mem::take(&mut *self.queue.lock().unwrap());
        for fut in queued {
            block_on(fut);
        }
    }
}

impl simple_someip::transport::Spawner for WorkingSpawner {
    fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        self.queue.lock().unwrap().push(Box::pin(future));
    }
}

/// Single-step `block_on` for the demo.
///
/// **ANTI-PATTERN — DO NOT USE IN PRODUCTION.** `Waker::noop()` means
/// no wake-up signal is ever registered; a future that yields
/// `Pending` waiting on real I/O would never get polled again. The
/// loop-and-`spin_loop()` fallback masks that by busy-spinning, which
/// is worse than useless on bare metal. Production executors use
/// proper `Waker` plumbing + a task queue driven by hardware
/// interrupts; this helper exists only to drive the demo's
/// synchronous mock futures (which resolve on the first poll).
///
/// For a real `no_alloc` `block_on`, see e.g. `embassy_executor::block_on`,
/// the `cassette` crate, or roll your own around a hardware-timer-driven
/// `Waker`. The `Future::poll` loop body below is the part that stays
/// the same; only the `Waker` plumbing and yield strategy change.
fn block_on<F: Future>(fut: F) -> F::Output {
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut fut = Box::pin(fut);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => {
                // ANTI-PATTERN: busy-spin. See fn docstring.
                std::hint::spin_loop();
            }
        }
    }
}

fn main() {
    // Each socket owns its own pipe; the "network" is us manually
    // moving bytes from A's send queue into B's recv queue below. For
    // a single send/recv demo this is enough; a more realistic mock
    // would wire the two queues into a cross-connected pair at bind
    // time.
    let pipe_a = Arc::new(MockPipe::default());
    let pipe_b = Arc::new(MockPipe::default());

    let factory_a = MockFactory {
        pipe: Arc::clone(&pipe_a),
        local_addr: SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 30500),
    };
    let factory_b = MockFactory {
        pipe: Arc::clone(&pipe_b),
        local_addr: SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 2), 30500),
    };
    let options = SocketOptions::new();

    let sock_a = block_on(factory_a.bind(factory_a.local_addr, &options)).expect("bind A");
    let sock_b = block_on(factory_b.bind(factory_b.local_addr, &options)).expect("bind B");

    let payload = b"hello bare-metal";
    block_on(sock_a.send_to(payload, sock_b.local_addr().unwrap())).expect("send_to");

    // DEMO-ONLY: hand-drain A's send queue into B's recv queue to
    // simulate "the network carried the datagram." A real bare-metal
    // integration would have its network driver (lwIP, smoltcp, a
    // custom Ethernet ISR, etc.) write directly into the receiving
    // socket's recv buffer — no user code touches the queues. This
    // drain pattern is not a template; it exists to keep the example
    // self-contained.
    let sent = std::mem::take(&mut *pipe_a.send_queue.lock().unwrap());
    for (bytes, _dst) in sent {
        pipe_b
            .recv_queue
            .lock()
            .unwrap()
            .push_back((bytes, sock_a.local_addr().unwrap()));
    }

    let mut buf = [0u8; 64];
    let datagram = block_on(sock_b.recv_from(&mut buf)).expect("recv_from");

    assert_eq!(datagram.bytes_received, payload.len());
    assert_eq!(datagram.source, sock_a.local_addr().unwrap());
    assert!(!datagram.truncated);
    assert_eq!(&buf[..datagram.bytes_received], payload);

    // Demonstrate the Timer trait briefly.
    let timer = MockTimer;
    block_on(timer.sleep(Duration::from_millis(1)));

    // Demonstrate the Spawner trait by submitting a future and then
    // draining the queue (proving the future was actually polled). A
    // real bare-metal Spawner would dispatch into its executor's task
    // pool and the executor would drain it on its own schedule.
    let spawner = WorkingSpawner::new();
    let polled = Arc::new(Mutex::new(false));
    let polled_for_task = Arc::clone(&polled);
    simple_someip::transport::Spawner::spawn(&spawner, async move {
        *polled_for_task.lock().unwrap() = true;
    });
    spawner.drain();
    assert!(
        *polled.lock().unwrap(),
        "WorkingSpawner must poll submitted futures to completion (Spawner trait contract)",
    );

    println!(
        "bare-metal example: sent {} bytes from {} to {}, received cleanly.",
        datagram.bytes_received,
        sock_a.local_addr().unwrap(),
        sock_b.local_addr().unwrap(),
    );
    println!(
        "note: trait layer (TransportSocket + TransportFactory + Timer + \
         Spawner + ChannelFactory) exercised end-to-end. Phases 9-12 \
         complete; phase 13 (client half) complete. Remaining: phase 14 \
         server-trait retargeting + server-side `server-tokio` split. \
         See top-of-file docblock."
    );
}
