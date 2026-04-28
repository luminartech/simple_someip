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
//! cargo build -p bare_metal
//! cargo run  -p bare_metal
//! ```
//!
//! # Patterns demonstrated
//!
//! | Pattern | This example | Firmware replacement |
//! |---------|-------------|----------------------|
//! | Channel factory | `BareMetalChannels` via `define_static_channels!` | same macro, sized to your HWM |
//! | Transport | `MockFactory` / `MockSocket` | `embassy_net`, smoltcp, custom Ethernet ISR |
//! | Timer | `MockTimer` using `tokio::task::yield_now` | `embassy_time::Timer::after` |
//! | Task spawner | `TokioBackedSpawner` | `embassy_executor::Spawner` |
//! | Lock handles | `Arc<Mutex<_>>` / `Arc<RwLock<_>>` | stack-allocated handles (see below) |
//!
//! # What is not yet demonstrated
//!
//! The `E2ERegistry` and interface handles still use heap-allocated
//! `Arc<Mutex<_>>` / `Arc<RwLock<_>>` wrappers. A future verification
//! pass will replace these with stack-allocated alternatives and confirm
//! zero heap allocation after `Client::new_with_deps` returns.
//!
//! [`Client::new_with_deps`]: simple_someip::Client::new_with_deps
//! [`ChannelFactory`]: simple_someip::transport::ChannelFactory

use core::future::Future;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::pin::Pin;
use core::task::{Context, Poll};
use core::time::Duration;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use simple_someip::client::{ClientUpdate, ControlMessage, ReceivedMessage, SendMessage};
use simple_someip::client::Error as ClientError;
use simple_someip::define_static_channels;
use simple_someip::e2e::E2ERegistry;
use simple_someip::protocol::sd::RebootFlag;
use simple_someip::transport::{
    ReceivedDatagram, SocketOptions, Spawner, Timer, TransportError, TransportFactory,
    TransportSocket,
};
use simple_someip::{Client, ClientDeps, RawPayload};

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

// ── Mock transport ────────────────────────────────────────────────────
//
// Two queues simulate the network. A real firmware transport drives
// these from a network driver ISR instead of an in-process VecDeque.

#[derive(Default)]
struct MockPipe {
    sent: Mutex<VecDeque<(Vec<u8>, SocketAddrV4)>>,
    inbound: Mutex<VecDeque<(Vec<u8>, SocketAddrV4)>>,
}

#[derive(Clone)]
struct MockFactory {
    pipe: Arc<MockPipe>,
    next_port: Arc<Mutex<u16>>,
}

impl TransportFactory for MockFactory {
    type Socket = MockSocket;

    fn bind(
        &self,
        addr: SocketAddrV4,
        _options: &SocketOptions,
    ) -> impl Future<Output = Result<Self::Socket, TransportError>> + Send {
        let pipe = Arc::clone(&self.pipe);
        let port = if addr.port() == 0 {
            let mut p = self.next_port.lock().unwrap();
            *p = p.saturating_add(1);
            30000u16.saturating_add(*p)
        } else {
            addr.port()
        };
        let local = SocketAddrV4::new(*addr.ip(), port);
        async move { Ok(MockSocket { pipe, local }) }
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
            // No datagram — wake immediately and yield. A real bare-metal
            // impl registers the waker on the network driver's RX-ready
            // interrupt instead of busy-waking.
            None => {
                cx.waker().wake_by_ref();
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
        MockRecvFut { pipe: Arc::clone(&self.pipe), buf }
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
// Uses tokio's yield_now to keep the example executor happy. Real
// firmware replaces this with e.g. `embassy_time::Timer::after(d).await`.

struct MockTimer;

impl Timer for MockTimer {
    async fn sleep(&self, _duration: Duration) {
        tokio::task::yield_now().await;
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

    // std Arc/Mutex/RwLock are sufficient here — they implement the
    // E2ERegistryHandle / InterfaceHandle lock-handle traits and are
    // gated by `feature = "std"`, not by `client-tokio`. A future
    // no-alloc port replaces these with stack-allocated handles.
    let e2e: Arc<Mutex<E2ERegistry>> = Arc::new(Mutex::new(E2ERegistry::new()));
    let iface: Arc<std::sync::RwLock<Ipv4Addr>> =
        Arc::new(std::sync::RwLock::new(Ipv4Addr::LOCALHOST));

    let (client, _updates, run_fut) = Client::<
        RawPayload,
        Arc<Mutex<E2ERegistry>>,
        Arc<std::sync::RwLock<Ipv4Addr>>,
        BareMetalChannels,
    >::new_with_deps(
        ClientDeps {
            factory,
            spawner: TokioBackedSpawner,
            timer: MockTimer,
            e2e_registry: e2e,
            interface: iface,
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
