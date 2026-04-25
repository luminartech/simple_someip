//! Host-side canary for the bare-metal trait surface.
//!
//! # What this example actually is
//!
//! A workspace-member binary that exercises `simple-someip`'s
//! `TransportSocket` / `TransportFactory` / `Timer` traits against a
//! hand-rolled mock backend. The `Cargo.toml` in this directory
//! depends on `simple-someip` with
//! `default-features = false, features = ["bare_metal"]`, so building
//! or running this example proves **that the trait surface compiles
//! under exactly the feature set a firmware consumer would use** —
//! no `std`-feature paths from `simple-someip`, no tokio, no socket2.
//! `cargo build --workspace` catches any regression that breaks this
//! surface even without running the binary.
//!
//! # How to run
//!
//! ```text
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
//! `SocketManager::bind*` today still pins `F::Socket = TokioSocket`,
//! so the trait impls below — while correct — cannot be plugged into
//! the crate's `Client` / `Server` event loops yet. Two upstream
//! blockers must land first:
//!
//! 1. Relax the `F::Socket = TokioSocket` bound to
//!    `F::Socket: TransportSocket` (requires stable Return-Type
//!    Notation or a GAT-based parallel trait).
//! 2. Extract a `Spawner` trait so `SocketManager::bind*` can submit
//!    per-socket loops to the user's executor instead of calling
//!    `tokio::spawn` directly. See phase 9 in the refactor plan.
//!
//! Until (1) and (2) land, bare-metal users CAN implement the traits
//! below, but they CANNOT route their implementations through
//! `Client` / `Server`.

use core::future::Future;
use core::net::{Ipv4Addr, SocketAddrV4};
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

impl TransportSocket for MockSocket {
    fn send_to(
        &mut self,
        buf: &[u8],
        target: SocketAddrV4,
    ) -> impl Future<Output = Result<(), TransportError>> {
        let bytes = buf.to_vec();
        let pipe = Arc::clone(&self.pipe);
        async move {
            pipe.send_queue.lock().unwrap().push_back((bytes, target));
            Ok(())
        }
    }

    fn recv_from(
        &mut self,
        buf: &mut [u8],
    ) -> impl Future<Output = Result<ReceivedDatagram, TransportError>> {
        let pipe = Arc::clone(&self.pipe);
        // Copy directly into `buf` by stealing its slice lifetime out
        // of the async block via a raw-pointer round-trip would be
        // unsafe; instead, poll the queue on first call and fill buf
        // synchronously if a datagram is ready. If the queue is empty,
        // this mock returns a ready
        // `Err(TransportError::Io(IoErrorKind::TimedOut))` rather than
        // a pending future. In this single-threaded example we always
        // send first then recv, so the timeout branch is unreachable
        // here.
        //
        // The mock borrow-dance is awkward compared to a real UDP
        // socket's recv_from; a production bare-metal impl would copy
        // bytes out of its driver's receive slab directly into `buf`.
        let result = {
            let mut q = pipe.recv_queue.lock().unwrap();
            q.pop_front()
        };
        match result {
            Some((bytes, source)) => {
                let n = bytes.len().min(buf.len());
                buf[..n].copy_from_slice(&bytes[..n]);
                core::future::ready(Ok(ReceivedDatagram {
                    bytes_received: n,
                    source,
                    truncated: n < bytes.len(),
                }))
            }
            None => core::future::ready(Err(TransportError::Io(IoErrorKind::TimedOut))),
        }
    }

    fn local_addr(&self) -> Result<SocketAddrV4, TransportError> {
        Ok(self.local_addr)
    }

    fn join_multicast_v4(
        &mut self,
        _group: Ipv4Addr,
        _iface: Ipv4Addr,
    ) -> Result<(), TransportError> {
        // Bare-metal stacks without multicast would return
        // Unsupported; our mock is happy to no-op.
        Ok(())
    }

    fn leave_multicast_v4(
        &mut self,
        _group: Ipv4Addr,
        _iface: Ipv4Addr,
    ) -> Result<(), TransportError> {
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

/// Single-step `block_on` for the demo.
///
/// **ANTI-PATTERN — DO NOT USE IN PRODUCTION.** `Waker::noop()` means
/// no wake-up signal is ever registered; a future that yields
/// `Pending` waiting on real I/O would never get polled again. The
/// loop-and-`spin_loop()` fallback here masks that by busy-spinning,
/// which is worse than useless on bare metal. Production executors
/// use proper `Waker` plumbing + a task queue driven by hardware
/// interrupts. This helper exists only to drive the demo's
/// synchronous mock futures (which resolve on the first poll).
fn block_on<F: Future>(fut: F) -> F::Output {
    let waker = Waker::noop();
    let mut cx = Context::from_waker(&waker);
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

    let mut sock_a = block_on(factory_a.bind(factory_a.local_addr, &options)).expect("bind A");
    let mut sock_b = block_on(factory_b.bind(factory_b.local_addr, &options)).expect("bind B");

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

    println!(
        "bare-metal example: sent {} bytes from {} to {}, received cleanly.",
        datagram.bytes_received,
        sock_a.local_addr().unwrap(),
        sock_b.local_addr().unwrap(),
    );
    println!(
        "note: this only exercises the trait layer — see source comments \
         for the Client/Server + Spawner gap (phase 9 work)."
    );
}
