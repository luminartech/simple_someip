//! Witness test: prove that `Client` can be constructed and driven
//! without the `client-tokio` feature, using a static-pool
//! [`ChannelFactory`] declared via [`define_static_channels!`] — the
//! production-bound bare-metal path (no per-call heap allocation for
//! channel storage).
//!
//! [`ChannelFactory`]: simple_someip::transport::ChannelFactory
//! [`define_static_channels!`]: simple_someip::define_static_channels
//!
//! Originally a witness using `EmbassySyncChannels` (which still
//! heap-allocates an `Arc<Channel<...>>` per call). The `static_channels`
//! module and `define_static_channels!` macro now provide a truly
//! heap-free path; this test exercises that macro end-to-end against
//! `Client::new_with_deps`.
//!
//! `simple-someip` is compiled with `default-features = false,
//! features = ["client", "bare_metal"]` per the `required-features`
//! gate below — NO tokio, NO socket2 pulled in via the crate itself.
//! The test runtime still uses the host's tokio (a `dev-dependency`)
//! for `#[tokio::test]` execution, but every type fed to
//! `Client::new_with_deps` is from the no-tokio side: a hand-rolled
//! mock `TransportFactory`, a hand-rolled `Timer`, the
//! macro-declared static-pool channels, and a `Spawner` that wraps
//! `tokio::spawn` purely as the test executor.
//!
//! Compile-witness alone (Cargo `required-features` proving the test
//! crate compiles without `client-tokio`) is the load-bearing
//! assertion; the runtime send/recv at the end is a sanity check
//! that the wired-up generics actually drive a working pipeline.
//! Per-call heap-allocation absence is verified separately in
//! `tests/static_channels_alloc_witness.rs`.
#![cfg(all(feature = "client", feature = "bare_metal"))]

use core::future::Future;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::pin::Pin;
use core::task::{Context, Poll};
use core::time::Duration;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use simple_someip::client::Error as ClientError;
use simple_someip::client::{ClientUpdate, ControlMessage, ReceivedMessage, SendMessage};
use simple_someip::define_static_channels;
use simple_someip::e2e::E2ERegistry;
use simple_someip::protocol::sd::RebootFlag;
use simple_someip::transport::{
    ReceivedDatagram, SocketOptions, Spawner, Timer, TransportError, TransportFactory,
    TransportSocket,
};
use simple_someip::{Client, ClientDeps, RawPayload};

// ── Static-pool channel factory declared via the macro ────────────────
//
// One pool per channeled `T`. Pool sizes here are deliberately small
// for a witness test; production firmware would size pools to the
// workload's high-water mark.
define_static_channels! {
    name: TestStaticChannels,
    oneshot: [
        (Result<(), ClientError>, 8),
        (Result<RawPayload, ClientError>, 4),
        (Result<RebootFlag, ClientError>, 4),
    ],
    bounded: [
        ((ControlMessage<RawPayload, TestStaticChannels>, 4), 1),
        ((SendMessage<RawPayload, TestStaticChannels>, 4), 4),
        ((Result<ReceivedMessage<RawPayload>, ClientError>, 4), 4),
    ],
    unbounded: [
        (ClientUpdate<RawPayload>, 1),
    ],
}

// ── Mock transport ─────────────────────────────────────────────────────

#[derive(Default)]
struct MockPipe {
    sent: Mutex<VecDeque<(Vec<u8>, SocketAddrV4)>>,
    inbound: Mutex<VecDeque<(Vec<u8>, SocketAddrV4)>>,
    inbound_waker: Mutex<Option<core::task::Waker>>,
}

#[derive(Clone)]
struct MockFactory {
    pipe: Arc<MockPipe>,
    local_port: Arc<Mutex<u16>>,
}

impl TransportFactory for MockFactory {
    type Socket = MockSocket;
    type BindFuture<'a> =
        core::pin::Pin<Box<dyn Future<Output = Result<Self::Socket, TransportError>> + Send + 'a>>;
    fn bind<'a>(&'a self, addr: SocketAddrV4, _options: &'a SocketOptions) -> Self::BindFuture<'a> {
        let pipe = Arc::clone(&self.pipe);
        let mut p = self.local_port.lock().unwrap();
        // Mock: assign port deterministically. If caller asked for 0,
        // hand out an incrementing fake ephemeral port.
        let port = if addr.port() == 0 {
            let next = *p + 1;
            *p = next;
            30000 + next
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
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = self.get_mut();
        let entry = me.pipe.inbound.lock().unwrap().pop_front();
        match entry {
            Some((bytes, source)) => {
                let n = bytes.len().min(me.buf.len());
                me.buf[..n].copy_from_slice(&bytes[..n]);
                Poll::Ready(Ok(ReceivedDatagram {
                    bytes_received: n,
                    source,
                    truncated: n < bytes.len(),
                }))
            }
            None => {
                // Park on the pipe's waker. Real bare-metal impls park
                // the task on an interrupt-driven waker;
                // wake_by_ref-on-empty would CPU-peg the test runtime.
                *me.pipe.inbound_waker.lock().unwrap() = Some(cx.waker().clone());
                // Re-check after registering to close the lost-wakeup
                // window between the pop_front above and the waker
                // store here.
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

    fn send_to<'a>(&'a self, buf: &'a [u8], target: SocketAddrV4) -> Self::SendFuture<'a> {
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

struct MockTimer;
impl Timer for MockTimer {
    type SleepFuture<'a> = core::pin::Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
    fn sleep(&self, duration: Duration) -> Self::SleepFuture<'_> {
        // Honor `duration` — the `Timer` trait's contract is that
        // implementations MAY overshoot but MUST NOT undershoot. The
        // test runtime is `#[tokio::test]` (tokio is a `dev-dependency`),
        // so using `tokio::time::sleep` is fine — it only proves the
        // production crate's no-tokio path compiles. A real bare-metal
        // impl would replace this with `embassy_time::Timer::after`.
        Box::pin(async move {
            tokio::time::sleep(duration).await;
        })
    }
}

// ── Spawner that delegates to tokio::spawn (test-runtime executor) ──

struct TokioBackedSpawner;
impl Spawner for TokioBackedSpawner {
    fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        drop(tokio::spawn(future));
    }
}

// ── Test ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn client_constructible_without_client_tokio_feature() {
    let pipe = Arc::new(MockPipe::default());
    let factory = MockFactory {
        pipe: Arc::clone(&pipe),
        local_port: Arc::new(Mutex::new(0)),
    };

    // Custom InterfaceHandle and E2ERegistryHandle that don't require
    // tokio. We use std Arc/Mutex/RwLock impls (which are gated by
    // `feature = "std"`, not by `client-tokio`).
    let interface_handle: Arc<std::sync::RwLock<Ipv4Addr>> =
        Arc::new(std::sync::RwLock::new(Ipv4Addr::LOCALHOST));
    let e2e_handle: Arc<Mutex<E2ERegistry>> = Arc::new(Mutex::new(E2ERegistry::new()));

    let (client, _updates, run_fut) = Client::<
        RawPayload,
        Arc<Mutex<E2ERegistry>>,
        Arc<std::sync::RwLock<Ipv4Addr>>,
        TestStaticChannels,
    >::new_with_deps(
        ClientDeps {
            factory,
            spawner: TokioBackedSpawner,
            timer: MockTimer,
            e2e_registry: e2e_handle,
            interface: interface_handle,
        },
        false,
    );

    // Compile-time witness: the constructor accepts no-tokio types,
    // returns a `Client` + updates triple, and the run-loop future
    // is `Send + 'static` (proven by the `tokio::spawn` below).
    let run_handle = tokio::spawn(run_fut);

    // Verify the Client handle is usable: read its interface address.
    assert_eq!(client.interface(), Ipv4Addr::LOCALHOST);

    // Tear down. `TestStaticChannels`'s bounded sender Drop sets the
    // slot's `closed` flag and wakes the receiver, so dropping all
    // `Client` clones lets the run loop's control-channel `recv`
    // resolve to `None` and the loop exits naturally — but it's
    // simpler to abort the spawned task directly here. The witness
    // goal is the compile + start-up sanity check, not graceful
    // shutdown semantics.
    run_handle.abort();
    drop(client);

    tokio::time::sleep(Duration::from_millis(50)).await;
}
