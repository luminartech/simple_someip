//! Host-side demonstration of [`Client::new_with_deps`] with a
//! static-pool no-alloc [`ChannelFactory`].
//!
//! # What this example shows
//!
//! `simple-someip` is compiled with
//! `default-features = false, features = ["client", "bare_metal"]` —
//! no tokio, no socket2 pulled in by *the crate itself*. The example
//! binary adds tokio only for its own executor and mock driver; real
//! firmware would use `embassy_executor` (or any bare-metal async
//! runtime) instead.
//!
//! Building or running this example in isolation proves that the
//! bare-metal API compiles under exactly the feature set a firmware
//! consumer would use:
//!
//! ```text
//! cargo build -p bare_metal_client
//! cargo run  -p bare_metal_client
//! ```
//!
//! # Patterns demonstrated
//!
//! | Pattern | This example | Firmware replacement |
//! |---------|-------------|----------------------|
//! | Channel factory | `BareMetalChannels` via `define_static_channels!` | same macro, sized to your HWM |
//! | Transport | `MockFactory` / `MockSocket` | `embassy_net`, smoltcp, custom Ethernet ISR |
//! | Timer | `MockTimer` using `tokio::time::sleep` | `embassy_time::Timer::after` |
//! | Task spawner | `TokioBackedSpawner` wrapping `tokio::spawn` | `embassy_executor::Spawner` |
//! | E2E registry handle | `StaticE2EHandle` over `&'static StaticE2EStorage` | same — already firmware-ready |
//! | Interface handle | `AtomicInterfaceHandle` over `&'static AtomicU32` | same — already firmware-ready |
//!
//! All five handle/factory types except `Transport` and `Timer` are the
//! actual `no_std` types you'd ship — `Static*` /
//! `Atomic*` over `&'static` storage. The transport and timer are
//! mocks because the example runs on the host; firmware swaps them
//! for embassy-net + embassy-time. `RawPayload` is std-only (it uses
//! a heap `Vec` for SD storage); a true firmware build provides its
//! own `PayloadWireFormat` impl.
//!
//! [`Client::new_with_deps`]: simple_someip::Client::new_with_deps
//! [`ChannelFactory`]: simple_someip::transport::ChannelFactory

use core::cell::RefCell;
use core::future::Future;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::pin::Pin;
use core::sync::atomic::AtomicU32;
use core::task::{Context, Poll};
use core::time::Duration;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use simple_someip::client::Error as ClientError;
use simple_someip::client::{ClientUpdate, ControlMessage, ReceivedMessage, SendMessage};
use simple_someip::define_static_channels;
use simple_someip::e2e::E2ERegistry;
use simple_someip::protocol::sd::RebootFlag;
use simple_someip::static_channels::BufferPool;
use simple_someip::transport::{
    ReceivedDatagram, SocketOptions, Spawner, StaticBufferProvider, Timer, TransportError,
    TransportFactory, TransportSocket,
};
use simple_someip::{AtomicInterfaceHandle, StaticE2EHandle, StaticE2EStorage};
use simple_someip::{Client, ClientDeps, RawPayload, UDP_BUFFER_SIZE};

// ── Static-pool channel factory ───────────────────────────────────────
//
// Pool sizes are sized to a modest single-service workload. Production
// firmware should size each pool to the workload's high-water mark
// (maximum concurrent in-flight requests / subscriptions).

define_static_channels! {
    name: BareMetalChannels,
    oneshot: [
        (Result<(), ClientError>, 8),
        (Result<RawPayload, ClientError>, 4),
        (Result<RebootFlag, ClientError>, 4),
    ],
    bounded: [
        ((ControlMessage<RawPayload, BareMetalChannels>, 4), 1),
        ((SendMessage<RawPayload, BareMetalChannels>, 16), 4),
        ((Result<ReceivedMessage<RawPayload>, ClientError>, 16), 4),
    ],
    unbounded: [
        (ClientUpdate<RawPayload>, 1),
    ],
}

// ── Bare-metal lock-handle storage ────────────────────────────────────
//
// `&'static` storage for the no-alloc lock handles. `E2ERegistry::new()`
// is `const`, so the storage lives in plain `static`s — no `Box::leak`
// required. On real firmware you'd write the same `static` declarations
// in boot code.

static E2E_STORAGE: StaticE2EStorage =
    BlockingMutex::<CriticalSectionRawMutex, RefCell<E2ERegistry>>::new(RefCell::new(
        E2ERegistry::new(),
    ));

// 127.0.0.1 packed as a big-endian u32.
static IFACE_STORAGE: AtomicU32 = AtomicU32::new(0x7F00_0001);

// ── Mock transport ────────────────────────────────────────────────────
//
// Two queues simulate the network. A real firmware transport drives
// these from a network driver ISR instead of an in-process VecDeque.

#[derive(Default)]
struct MockPipe {
    sent: Mutex<VecDeque<(Vec<u8>, SocketAddrV4)>>,
    inbound: Mutex<VecDeque<(Vec<u8>, SocketAddrV4)>>,
    inbound_waker: Mutex<Option<core::task::Waker>>,
}

#[derive(Clone)]
struct MockFactory {
    pipe: Arc<MockPipe>,
    next_port: Arc<Mutex<u16>>,
}

impl TransportFactory for MockFactory {
    type Socket = MockSocket;
    type BindFuture<'a> =
        core::pin::Pin<Box<dyn Future<Output = Result<Self::Socket, TransportError>> + Send + 'a>>;

    fn bind<'a>(&'a self, addr: SocketAddrV4, _options: &'a SocketOptions) -> Self::BindFuture<'a> {
        let pipe = Arc::clone(&self.pipe);
        let port = if addr.port() == 0 {
            let mut p = self.next_port.lock().unwrap();
            *p = p.saturating_add(1);
            30000u16.saturating_add(*p)
        } else {
            addr.port()
        };
        let local = SocketAddrV4::new(*addr.ip(), port);
        Box::pin(async move { Ok(MockSocket { pipe, local }) })
    }
}

struct MockSocket {
    pipe: Arc<MockPipe>,
    local: SocketAddrV4,
}

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
            me.pipe.sent.lock().unwrap().push_back((bytes, me.target));
        }
        Poll::Ready(Ok(()))
    }
}

struct MockRecvFut<'a> {
    pipe: Arc<MockPipe>,
    buf: &'a mut [u8],
}

impl Future for MockRecvFut<'_> {
    type Output = Result<ReceivedDatagram, TransportError>;

    #[allow(clippy::single_match_else)]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = self.get_mut();
        match me.pipe.inbound.lock().unwrap().pop_front() {
            Some((bytes, source)) => {
                let n = bytes.len().min(me.buf.len());
                me.buf[..n].copy_from_slice(&bytes[..n]);
                Poll::Ready(Ok(ReceivedDatagram {
                    bytes_received: n,
                    source,
                    truncated: n < bytes.len(),
                }))
            }
            // No datagram — register the waker on the pipe and park.
            // A real bare-metal impl registers the waker on the network
            // driver's RX-ready interrupt instead.
            None => {
                *me.pipe.inbound_waker.lock().unwrap() = Some(cx.waker().clone());
                if let Some((bytes, source)) = me.pipe.inbound.lock().unwrap().pop_front() {
                    let n = bytes.len().min(me.buf.len());
                    me.buf[..n].copy_from_slice(&bytes[..n]);
                    return Poll::Ready(Ok(ReceivedDatagram {
                        bytes_received: n,
                        source,
                        truncated: n < bytes.len(),
                    }));
                }
                Poll::Pending
            }
        }
    }
}

impl TransportSocket for MockSocket {
    type SendFuture<'a> = MockSendFut;
    type RecvFuture<'a> = MockRecvFut<'a>;

    fn send_to<'a>(&'a self, buf: &'a [u8], target: SocketAddrV4) -> MockSendFut {
        MockSendFut {
            pipe: Arc::clone(&self.pipe),
            bytes: Some(buf.to_vec()),
            target,
        }
    }

    fn recv_from<'a>(&'a self, buf: &'a mut [u8]) -> MockRecvFut<'a> {
        MockRecvFut {
            pipe: Arc::clone(&self.pipe),
            buf,
        }
    }

    fn local_addr(&self) -> Result<SocketAddrV4, TransportError> {
        Ok(self.local)
    }

    fn join_multicast_v4(&self, _group: Ipv4Addr, _iface: Ipv4Addr) -> Result<(), TransportError> {
        Ok(())
    }

    fn leave_multicast_v4(&self, _group: Ipv4Addr, _iface: Ipv4Addr) -> Result<(), TransportError> {
        Ok(())
    }
}

// ── Mock Timer ────────────────────────────────────────────────────────
//
// Honors `duration` per the `Timer` trait contract (MAY overshoot, MUST
// NOT undershoot). Real firmware replaces this with e.g.
// `embassy_time::Timer::after(d).await`.

struct MockTimer;

impl Timer for MockTimer {
    type SleepFuture<'a> = core::pin::Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
    fn sleep(&self, duration: Duration) -> Self::SleepFuture<'_> {
        Box::pin(async move {
            tokio::time::sleep(duration).await;
        })
    }
}

// ── Spawner ───────────────────────────────────────────────────────────
//
// Wraps tokio::spawn for this example. Real firmware wraps
// `embassy_executor::Spawner::spawn` or equivalent. The Spawner trait
// contract requires submitted futures to be polled to completion —
// never drop them without polling.

struct TokioBackedSpawner;

impl Spawner for TokioBackedSpawner {
    fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        drop(tokio::spawn(future));
    }
}

// ── Main ──────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let pipe = Arc::new(MockPipe::default());
    let factory = MockFactory {
        pipe: Arc::clone(&pipe),
        next_port: Arc::new(Mutex::new(0)),
    };

    // Bare-metal lock handles: both pure no_std (no allocator), each
    // backed by a `&'static` storage. The `static`s themselves are
    // declared at module scope (see top of file) — clippy::pedantic
    // dislikes `static` after `let` statements.
    let e2e = StaticE2EHandle::new(&E2E_STORAGE);
    let iface = AtomicInterfaceHandle::new(&IFACE_STORAGE);

    let (client, _updates, run_fut) = Client::<
        RawPayload,
        StaticE2EHandle,
        AtomicInterfaceHandle,
        BareMetalChannels,
    >::new_with_deps(
        ClientDeps {
            factory,
            spawner: TokioBackedSpawner,
            timer: MockTimer,
            e2e_registry: e2e,
            interface: iface,
            // Caller-declared static buffer pool (#125): UNICAST_SOCKETS_CAP
            // (8) + 1 discovery + 1 release-lag slack = 10 slots. An evicted
            // socket's lease frees asynchronously, so size one above the max
            // live socket count to avoid a transient Capacity("udp_buffer")
            // on evict-then-rebind. On real firmware this is a `static`; here
            // it is a function-local `static` for the example.
            buffer_provider: {
                static POOL: BufferPool<10, UDP_BUFFER_SIZE> = BufferPool::new();
                StaticBufferProvider(&POOL)
            },
        },
        false, // multicast_loopback
    );
    // `_updates` is a `ClientUpdates` receiver. In production, poll it
    // for `ClientUpdate` events: discovery changes, unicast replies,
    // reboot notifications, and errors.

    // The run future is Send + 'static, so it can be handed to any
    // executor — tokio here, embassy_executor on real firmware.
    let run_handle = tokio::spawn(run_fut);

    // Client is live. Sanity-check the interface address.
    assert_eq!(client.interface(), Ipv4Addr::LOCALHOST);

    // Tear down: drop client first (closes the control channel), then
    // abort and await cancellation.
    drop(client);
    run_handle.abort();
    let _ = run_handle.await;

    println!(
        "bare-metal example: Client::new_with_deps with BareMetalChannels (define_static_channels!) \
         compiled and ran successfully under features=[client, bare_metal] — \
         no tokio / socket2 from simple-someip itself."
    );
}
