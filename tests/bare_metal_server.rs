//! Witness test: prove that `Server` can be constructed and
//! driven without the `server-tokio` feature, using only the trait
//! surface (`TransportFactory`, `Timer`, `E2ERegistryHandle`,
//! `SubscriptionHandle`).
//!
//! `simple-someip` is compiled with `default-features = false,
//! features = ["server", "bare_metal"]` per the `required-features`
//! gate below — i.e. NO tokio, NO socket2 pulled in via the crate
//! itself. The test still uses the host's tokio runtime as a generic
//! executor (tokio is a `dev-dependency`), but every type fed to
//! `simple-someip::Server::new_with_deps` comes from the no-tokio side:
//! a hand-rolled mock `TransportFactory`, a hand-rolled `Timer`, a
//! hand-rolled `SubscriptionHandle`, and the std-backed
//! `Arc<Mutex<E2ERegistry>>` impl that ships under the bare `transport`
//! module.
//!
//! This is the gate witness for the claim that `Server` is reachable
//! on a no-tokio build. Compile-witness alone (Cargo `required-features`
//! proving the test crate compiles without `server-tokio`) is the
//! load-bearing assertion; the `tokio::spawn` at the end is a sanity
//! check that the announcement-loop future is `Send + 'static` and
//! the trait surface drives a working pipeline.
#![cfg(all(feature = "server", feature = "bare_metal"))]

use core::future::Future;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::pin::Pin;
use core::task::{Context, Poll};
use core::time::Duration;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::vec::Vec;

use simple_someip::e2e::E2ERegistry;
use simple_someip::server::NonSdRequestCallback;
use simple_someip::server::ServerConfig;
use simple_someip::server::{SubscribeError, Subscriber, SubscriptionHandle};
use simple_someip::transport::{
    ReceivedDatagram, SocketOptions, Timer, TransportError, TransportFactory, TransportSocket,
};
use simple_someip::{Server, ServerDeps};

// ── Mock transport ─────────────────────────────────────────────────────

#[derive(Default)]
struct MockPipe {
    sent: Mutex<VecDeque<(Vec<u8>, SocketAddrV4)>>,
    inbound: Mutex<VecDeque<(Vec<u8>, SocketAddrV4)>>,
    inbound_waker: Mutex<Option<core::task::Waker>>,
}

/// Per-socket pipes routed by bind options: with a single shared
/// queue, whichever `select_biased!` arm polled first stole the
/// datagram, so `recv_loop`'s `from_unicast` flag depended on the
/// alternating bias — per-socket queues make routing deterministic.
#[derive(Clone)]
struct MockFactory {
    /// Handed to sockets bound WITHOUT multicast options — the
    /// server's unicast service socket.
    unicast_pipe: Arc<MockPipe>,
    /// Handed to sockets bound WITH `multicast_if_v4` set — the
    /// active server's SD socket. NOTE: passive servers bind their SD
    /// placeholder without multicast options, so BOTH passive sockets
    /// share `unicast_pipe` and this pipe goes unused.
    sd_pipe: Arc<MockPipe>,
    next_port: Arc<Mutex<u16>>,
}

impl TransportFactory for MockFactory {
    type Socket = MockSocket;
    type BindFuture<'a> =
        core::pin::Pin<Box<dyn Future<Output = Result<Self::Socket, TransportError>> + Send + 'a>>;
    fn bind<'a>(&'a self, addr: SocketAddrV4, options: &'a SocketOptions) -> Self::BindFuture<'a> {
        let pipe = if options.multicast_if_v4.is_some() {
            Arc::clone(&self.sd_pipe)
        } else {
            Arc::clone(&self.unicast_pipe)
        };
        // Mock: assign port deterministically. If caller asked for 0,
        // hand out an incrementing fake ephemeral port.
        let port = if addr.port() == 0 {
            let mut p = self.next_port.lock().unwrap();
            let next = *p + 1;
            *p = next;
            40000 + next
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

#[derive(Clone)]
struct MockTimer;
impl Timer for MockTimer {
    type SleepFuture<'a> = core::pin::Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
    fn sleep(&self, duration: Duration) -> Self::SleepFuture<'_> {
        // Honor `duration` per the `Timer` trait contract (MAY
        // overshoot, MUST NOT undershoot). The test runtime is
        // `#[tokio::test]`; this only demonstrates the no-tokio
        // production path compiles. A real bare-metal impl would
        // replace this with `embassy_time::Timer::after`.
        Box::pin(async move {
            tokio::time::sleep(duration).await;
        })
    }
}

// ── Mock SubscriptionHandle ───────────────────────────────────────────
//
// On `server-tokio`, `Arc<RwLock<SubscriptionManager>>` is a built-in
// impl. Bare-metal callers supply their own. A real bare-metal impl
// would back this with a `critical_section::Mutex<RefCell<...>>` or a
// `spin::Mutex<...>` over a `heapless`-backed table; here we use
// `std::sync::Mutex` over a tiny inline table because the test runtime
// has `std`. The point is the *trait* impl, not the concurrency
// primitive.

type SubKey = (u16, u16, u16, SocketAddrV4);

#[derive(Clone, Default)]
#[allow(clippy::type_complexity)]
struct MockSubscriptions(Arc<Mutex<Vec<SubKey>>>);

impl SubscriptionHandle for MockSubscriptions {
    type SubscribeFuture<'a> =
        core::pin::Pin<Box<dyn Future<Output = Result<(), SubscribeError>> + Send + 'a>>;
    type UnsubscribeFuture<'a> = core::pin::Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

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
    ) -> impl Future<Output = usize> + 'a
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

// ── Test ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn server_constructible_without_server_tokio_feature() {
    let factory = MockFactory {
        unicast_pipe: Arc::new(MockPipe::default()),
        sd_pipe: Arc::new(MockPipe::default()),
        next_port: Arc::new(Mutex::new(0)),
    };

    let e2e_handle: Arc<Mutex<E2ERegistry>> = Arc::new(Mutex::new(E2ERegistry::new()));
    let subs = MockSubscriptions::default();

    let config = ServerConfig::new(0x5B, 1)
        .with_interface(Ipv4Addr::LOCALHOST)
        .with_local_port(30490);

    let deps: ServerDeps<MockFactory, MockTimer, Arc<Mutex<E2ERegistry>>, MockSubscriptions> =
        ServerDeps {
            factory,
            timer: MockTimer,
            e2e_registry: e2e_handle,
            subscriptions: subs,
            non_sd_observer: None,
        };

    let (_server, _handles, run): (
        Server<MockFactory, MockTimer, Arc<Mutex<E2ERegistry>>, MockSubscriptions>,
        _,
        _,
    ) = Server::new_with_deps(deps, config, false)
        .await
        .expect("Server::new_with_deps must succeed with no-tokio mocks");

    // The combined run-future drives both receive and announce.
    // Spawning it on tokio proves it's `'static`. The witness is
    // purely structural: if this line compiles, `Server` is reachable
    // on a no-tokio build.
    let handle = tokio::spawn(run);

    // Yield once so the spawned future has a chance to poll (its first
    // tick fires `send_to` immediately, before the timer sleep).
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // Tear down: abort the announce loop.
    handle.abort();
    let _ = handle.await;
}

#[tokio::test]
async fn passive_server_constructible_without_server_tokio_feature() {
    let factory = MockFactory {
        unicast_pipe: Arc::new(MockPipe::default()),
        sd_pipe: Arc::new(MockPipe::default()),
        next_port: Arc::new(Mutex::new(0)),
    };

    let e2e_handle: Arc<Mutex<E2ERegistry>> = Arc::new(Mutex::new(E2ERegistry::new()));
    let subs = MockSubscriptions::default();

    let config = ServerConfig::new(0x5C, 2)
        .with_interface(Ipv4Addr::LOCALHOST)
        .with_local_port(0);

    let deps: ServerDeps<MockFactory, MockTimer, Arc<Mutex<E2ERegistry>>, MockSubscriptions> =
        ServerDeps {
            factory,
            timer: MockTimer,
            e2e_registry: e2e_handle,
            subscriptions: subs,
            non_sd_observer: None,
        };

    let (_server, _handles, _run): (
        Server<MockFactory, MockTimer, Arc<Mutex<E2ERegistry>>, MockSubscriptions>,
        _,
        _,
    ) = Server::new_passive_with_deps(deps, config)
        .await
        .expect("Server::new_passive_with_deps must succeed with no-tokio mocks");
}

// ── NonSdRequestCallback witness ──────────────────────────────────────
//
// Drives datagrams through the server's `recv_loop` and checks the
// observer contract: `NonSdRequestCallback` is
// `fn(ctx: usize, source: SocketAddrV4, service_id: u16, method_id: u16,
//     payload: &[u8], e2e_status: u8)` — a plain function pointer, so
// it can't capture environment. `recv_loop` parses the SOME/IP header
// and passes decoded fields; the consumer never sees raw datagram bytes.
// Each test parks its observation in a dedicated `OnceLock`-backed
// static to avoid interference under parallel `cargo test`.
//
// Positive test: registered observer fires for non-SD unicast.
// Negative tests: registered observer must NOT fire for SD messages
// (regardless of socket) or for non-SD datagrams on the SD socket.
// None test: with no observer registered, a non-SD unicast is processed
// without panicking (no callback to witness — just a no-panic guarantee).

use std::sync::OnceLock;

static OBSERVED_SOME: OnceLock<Mutex<Option<(usize, SocketAddrV4, u16, u16, Vec<u8>, u8)>>> =
    OnceLock::new();

fn record_some(
    ctx: usize,
    source: SocketAddrV4,
    service_id: u16,
    method_id: u16,
    payload: &[u8],
    e2e_status: u8,
    _response_out: &mut [u8],
) -> i32 {
    let slot = OBSERVED_SOME.get_or_init(|| Mutex::new(None));
    *slot.lock().unwrap() = Some((
        ctx,
        source,
        service_id,
        method_id,
        payload.to_vec(),
        e2e_status,
    ));
    -1 // observer only — no response
}

static OBSERVED_SD_UNICAST: OnceLock<Mutex<Option<(usize, SocketAddrV4, u16, u16, Vec<u8>, u8)>>> =
    OnceLock::new();
static OBSERVED_MULTICAST: OnceLock<Mutex<Option<(usize, SocketAddrV4, u16, u16, Vec<u8>, u8)>>> =
    OnceLock::new();

fn record_sd_unicast(
    ctx: usize,
    source: SocketAddrV4,
    service_id: u16,
    method_id: u16,
    payload: &[u8],
    e2e_status: u8,
    _response_out: &mut [u8],
) -> i32 {
    let slot = OBSERVED_SD_UNICAST.get_or_init(|| Mutex::new(None));
    *slot.lock().unwrap() = Some((
        ctx,
        source,
        service_id,
        method_id,
        payload.to_vec(),
        e2e_status,
    ));
    -1 // observer only — no response
}

fn record_multicast(
    ctx: usize,
    source: SocketAddrV4,
    service_id: u16,
    method_id: u16,
    payload: &[u8],
    e2e_status: u8,
    _response_out: &mut [u8],
) -> i32 {
    let slot = OBSERVED_MULTICAST.get_or_init(|| Mutex::new(None));
    *slot.lock().unwrap() = Some((
        ctx,
        source,
        service_id,
        method_id,
        payload.to_vec(),
        e2e_status,
    ));
    -1 // observer only — no response
}

/// Build a minimal SOME/IP method-request datagram (16-byte header,
/// no payload, message_type = Request). The exact byte layout matches
/// the on-wire SOME/IP header format documented in
/// AUTOSAR_SWS_SOMEIPProtocol §4 — checked-by-encode against
/// `simple_someip::protocol::Header::encode` would be cleaner but the
/// header is small enough to spell out by hand and avoids dragging
/// the encoder dep into the test.
fn build_method_request(service_id: u16, method_id: u16, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(16 + payload.len());
    buf.extend_from_slice(&service_id.to_be_bytes()); // message_id (high)
    buf.extend_from_slice(&method_id.to_be_bytes()); //  message_id (low)
    buf.extend_from_slice(&(8u32 + payload.len() as u32).to_be_bytes()); // length = header(8) + payload
    buf.extend_from_slice(&0u32.to_be_bytes()); //       request_id
    buf.push(1); //                                       protocol_version
    buf.push(1); //                                       interface_version
    buf.push(0); //                                       message_type = Request (0x00)
    buf.push(0); //                                       return_code = OK
    buf.extend_from_slice(payload);
    buf
}

/// Build a minimal, well-formed SOME/IP-SD datagram: SD message id
/// (0xFFFF / 0x8100), then an SD payload with flags + reserved and
/// ZERO entries / options. Routing-wise a legitimate (if vacuous) SD
/// message — `recv_loop` must hand it to SD handling, never to the
/// non-SD observer, regardless of which socket it arrived on.
fn build_sd_message() -> Vec<u8> {
    let mut buf = Vec::with_capacity(28);
    buf.extend_from_slice(&0xFFFFu16.to_be_bytes()); // message_id (high): SD service
    buf.extend_from_slice(&0x8100u16.to_be_bytes()); // message_id (low): SD method
    buf.extend_from_slice(&20u32.to_be_bytes()); //     length = header(8) + sd payload(12)
    buf.extend_from_slice(&0u32.to_be_bytes()); //      request_id
    buf.push(1); //                                      protocol_version
    buf.push(1); //                                      interface_version
    buf.push(2); //                                      message_type = Notification (0x02)
    buf.push(0); //                                      return_code = OK
    buf.push(0x80); //                                   SD flags: reboot
    buf.extend_from_slice(&[0, 0, 0]); //                reserved
    buf.extend_from_slice(&0u32.to_be_bytes()); //       entries array length = 0
    buf.extend_from_slice(&0u32.to_be_bytes()); //       options array length = 0
    buf
}

async fn drive_until<F: FnMut() -> bool>(mut check: F) {
    // Yield enough times for the spawned run-future to pick up the
    // queued inbound datagram from the mock pipe. The mock socket's
    // `recv_from` resolves immediately when a datagram is queued, so a
    // few yields are sufficient on the multi-threaded runtime; we
    // bound it at 200 yields (~ms) to keep the test snappy.
    for _ in 0..200 {
        if check() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("timed out waiting for condition (callback never fired or assertion never held)");
}

#[tokio::test]
async fn non_sd_observer_some_receives_unicast_method_request() {
    let unicast_pipe = Arc::new(MockPipe::default());
    let factory = MockFactory {
        unicast_pipe: Arc::clone(&unicast_pipe),
        sd_pipe: Arc::new(MockPipe::default()),
        next_port: Arc::new(Mutex::new(0)),
    };

    let e2e_handle: Arc<Mutex<E2ERegistry>> = Arc::new(Mutex::new(E2ERegistry::new()));
    let subs = MockSubscriptions::default();

    let config = ServerConfig::new(0x1234, 1)
        .with_interface(Ipv4Addr::LOCALHOST)
        .with_local_port(30700);

    let deps: ServerDeps<MockFactory, MockTimer, Arc<Mutex<E2ERegistry>>, MockSubscriptions> =
        ServerDeps {
            factory,
            timer: MockTimer,
            e2e_registry: e2e_handle,
            subscriptions: subs,
            non_sd_observer: Some((record_some as NonSdRequestCallback, 0xC0FF_EE00)),
        };

    let (_server, _handles, run): (
        Server<MockFactory, MockTimer, Arc<Mutex<E2ERegistry>>, MockSubscriptions>,
        _,
        _,
    ) = Server::new_with_deps(deps, config, false)
        .await
        .expect("Server::new_with_deps must succeed");

    let handle = tokio::spawn(run);

    // Queue a non-SD unicast method-request datagram on the unicast
    // socket's own pipe; per-socket pipes make `from_unicast = true`
    // deterministic.
    let datagram = build_method_request(0x1234, 0x0001, &[0xDE, 0xAD, 0xBE, 0xEF]);
    let src = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 100), 40000);
    unicast_pipe
        .inbound
        .lock()
        .unwrap()
        .push_back((datagram.clone(), src));
    if let Some(w) = unicast_pipe.inbound_waker.lock().unwrap().take() {
        w.wake();
    }

    drive_until(|| {
        OBSERVED_SOME
            .get()
            .and_then(|m| m.lock().unwrap().clone())
            .is_some()
    })
    .await;

    let (got_ctx, got_src, got_service, got_method, got_payload, got_e2e) = OBSERVED_SOME
        .get()
        .unwrap()
        .lock()
        .unwrap()
        .clone()
        .expect("callback fired");
    assert_eq!(
        got_ctx, 0xC0FF_EE00,
        "callback must receive the registered ctx word verbatim"
    );
    assert_eq!(got_src, src, "callback must receive the original source");
    assert_eq!(got_service, 0x1234, "decoded service id");
    assert_eq!(got_method, 0x0001, "decoded method id");
    assert_eq!(
        got_payload,
        [0xDE, 0xAD, 0xBE, 0xEF],
        "payload must be the bytes after the 16-byte header"
    );
    assert_eq!(got_e2e, 0, "server-side requests are not E2E-checked today");

    handle.abort();
    let _ = handle.await;
}

/// A registered observer must NOT fire for an SD message arriving on
/// the unicast socket — SD-formatted unicast traffic (e.g. unicast
/// FindService) routes to SD handling. Unlike the pre-PR-1 negative
/// test, the witness callback IS registered, so a routing regression
/// (SD datagrams leaking to the observer) trips the assertion.
#[tokio::test]
async fn non_sd_observer_ignores_sd_message_on_unicast_socket() {
    let unicast_pipe = Arc::new(MockPipe::default());
    let factory = MockFactory {
        unicast_pipe: Arc::clone(&unicast_pipe),
        sd_pipe: Arc::new(MockPipe::default()),
        next_port: Arc::new(Mutex::new(0)),
    };

    let e2e_handle: Arc<Mutex<E2ERegistry>> = Arc::new(Mutex::new(E2ERegistry::new()));
    let config = ServerConfig::new(0x1234, 1)
        .with_interface(Ipv4Addr::LOCALHOST)
        .with_local_port(30702);

    let deps: ServerDeps<MockFactory, MockTimer, Arc<Mutex<E2ERegistry>>, MockSubscriptions> =
        ServerDeps {
            factory,
            timer: MockTimer,
            e2e_registry: e2e_handle,
            subscriptions: MockSubscriptions::default(),
            non_sd_observer: Some((record_sd_unicast as NonSdRequestCallback, 7)),
        };

    let (_server, _handles, run): (
        Server<MockFactory, MockTimer, Arc<Mutex<E2ERegistry>>, MockSubscriptions>,
        _,
        _,
    ) = Server::new_with_deps(deps, config, false)
        .await
        .expect("Server::new_with_deps must succeed");
    let handle = tokio::spawn(run);

    let src = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 102), 40002);
    unicast_pipe
        .inbound
        .lock()
        .unwrap()
        .push_back((build_sd_message(), src));
    if let Some(w) = unicast_pipe.inbound_waker.lock().unwrap().take() {
        w.wake();
    }

    // Deterministic completion signal: wait until the run-future has
    // consumed the datagram from the pipe, then yield once more. The
    // observer path has no await point between dequeue and callback,
    // so any leaked invocation has already happened by now.
    drive_until(|| unicast_pipe.inbound.lock().unwrap().is_empty()).await;
    tokio::task::yield_now().await;
    assert!(
        !handle.is_finished(),
        "run-future must still be alive after processing the datagram"
    );

    let observed = OBSERVED_SD_UNICAST
        .get()
        .and_then(|m| m.lock().unwrap().clone());
    assert!(
        observed.is_none(),
        "observer must NOT fire for SD messages; got {observed:?}"
    );
    handle.abort();
    let _ = handle.await;
}

/// A registered observer must NOT fire for a non-SD datagram arriving
/// on the SD/multicast socket — the observer contract is unicast-only
/// (`from_unicast == true`).
#[tokio::test]
async fn non_sd_observer_ignores_non_sd_on_multicast_socket() {
    let sd_pipe = Arc::new(MockPipe::default());
    let factory = MockFactory {
        unicast_pipe: Arc::new(MockPipe::default()),
        sd_pipe: Arc::clone(&sd_pipe),
        next_port: Arc::new(Mutex::new(0)),
    };

    let e2e_handle: Arc<Mutex<E2ERegistry>> = Arc::new(Mutex::new(E2ERegistry::new()));
    let config = ServerConfig::new(0x1234, 1)
        .with_interface(Ipv4Addr::LOCALHOST)
        .with_local_port(30703);

    let deps: ServerDeps<MockFactory, MockTimer, Arc<Mutex<E2ERegistry>>, MockSubscriptions> =
        ServerDeps {
            factory,
            timer: MockTimer,
            e2e_registry: e2e_handle,
            subscriptions: MockSubscriptions::default(),
            non_sd_observer: Some((record_multicast as NonSdRequestCallback, 9)),
        };

    let (_server, _handles, run): (
        Server<MockFactory, MockTimer, Arc<Mutex<E2ERegistry>>, MockSubscriptions>,
        _,
        _,
    ) = Server::new_with_deps(deps, config, false)
        .await
        .expect("Server::new_with_deps must succeed");
    let handle = tokio::spawn(run);

    let src = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 103), 40003);
    sd_pipe
        .inbound
        .lock()
        .unwrap()
        .push_back((build_method_request(0x1234, 0x0001, &[]), src));
    if let Some(w) = sd_pipe.inbound_waker.lock().unwrap().take() {
        w.wake();
    }

    // Deterministic completion signal: wait until the run-future has
    // consumed the datagram from the pipe, then yield once more. The
    // observer path has no await point between dequeue and callback,
    // so any leaked invocation has already happened by now.
    drive_until(|| sd_pipe.inbound.lock().unwrap().is_empty()).await;
    tokio::task::yield_now().await;
    assert!(
        !handle.is_finished(),
        "run-future must still be alive after processing the datagram"
    );

    let observed = OBSERVED_MULTICAST
        .get()
        .and_then(|m| m.lock().unwrap().clone());
    assert!(
        observed.is_none(),
        "observer must NOT fire for non-unicast datagrams; got {observed:?}"
    );
    handle.abort();
    let _ = handle.await;
}

/// With `non_sd_observer: None`, a non-SD unicast datagram is processed
/// without panicking (historical "ignore" behavior). This is all the
/// `None` case can actually prove — there is no callback to witness.
#[tokio::test]
async fn non_sd_observer_none_preserves_ignore_behavior() {
    let unicast_pipe = Arc::new(MockPipe::default());
    let factory = MockFactory {
        unicast_pipe: Arc::clone(&unicast_pipe),
        sd_pipe: Arc::new(MockPipe::default()),
        next_port: Arc::new(Mutex::new(0)),
    };

    let e2e_handle: Arc<Mutex<E2ERegistry>> = Arc::new(Mutex::new(E2ERegistry::new()));
    let config = ServerConfig::new(0x1234, 1)
        .with_interface(Ipv4Addr::LOCALHOST)
        .with_local_port(30701);

    let deps: ServerDeps<MockFactory, MockTimer, Arc<Mutex<E2ERegistry>>, MockSubscriptions> =
        ServerDeps {
            factory,
            timer: MockTimer,
            e2e_registry: e2e_handle,
            subscriptions: MockSubscriptions::default(),
            non_sd_observer: None,
        };

    let (_server, _handles, run): (
        Server<MockFactory, MockTimer, Arc<Mutex<E2ERegistry>>, MockSubscriptions>,
        _,
        _,
    ) = Server::new_with_deps(deps, config, false)
        .await
        .expect("Server::new_with_deps must succeed");
    let handle = tokio::spawn(run);

    let src = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 101), 40001);
    unicast_pipe
        .inbound
        .lock()
        .unwrap()
        .push_back((build_method_request(0x1234, 0x0001, &[]), src));
    if let Some(w) = unicast_pipe.inbound_waker.lock().unwrap().take() {
        w.wake();
    }

    // Deterministic completion signal: wait until the run-future has
    // consumed the datagram from the pipe, then yield once more. The
    // observer path has no await point between dequeue and callback,
    // so any leaked invocation has already happened by now.
    drive_until(|| unicast_pipe.inbound.lock().unwrap().is_empty()).await;
    tokio::task::yield_now().await;

    assert!(
        !handle.is_finished(),
        "run-future must keep running (no panic / no error) after \
         ignoring a non-SD datagram with no observer registered"
    );
    handle.abort();
    let _ = handle.await;
}

// ── Responder (getter) path ───────────────────────────────────────────
//
// A non-negative return from `NonSdRequestCallback` means "I wrote a
// `len`-byte response into `response_out`"; the server frames a SOME/IP
// RESPONSE (echoing the request id) and sends it back to the source.
// `record_*` above all return -1, so these two tests are the only
// coverage of the framing branch and its length-guard.

/// Build a method request with an explicit `request_id` so the response
/// path's id-echo can be asserted. Mirrors `build_method_request` but
/// threads `request_id` instead of hardcoding 0.
fn build_method_request_with_id(
    service_id: u16,
    method_id: u16,
    request_id: u32,
    payload: &[u8],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(16 + payload.len());
    buf.extend_from_slice(&service_id.to_be_bytes());
    buf.extend_from_slice(&method_id.to_be_bytes());
    buf.extend_from_slice(&(8u32 + payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(&request_id.to_be_bytes());
    buf.push(1); // protocol_version
    buf.push(1); // interface_version
    buf.push(0); // message_type = Request
    buf.push(0); // return_code = OK
    buf.extend_from_slice(payload);
    buf
}

/// Getter responder: writes a fixed 3-byte body into `response_out` and
/// returns its length.
fn respond_with_body(
    _ctx: usize,
    _source: SocketAddrV4,
    _service_id: u16,
    _method_id: u16,
    _payload: &[u8],
    _e2e_status: u8,
    response_out: &mut [u8],
) -> i32 {
    let body = [0x11u8, 0x22, 0x33];
    response_out[..body.len()].copy_from_slice(&body);
    body.len() as i32
}

/// Contract-violating responder: claims more bytes than the buffer it
/// was handed actually holds. The server must reject this without
/// panicking or emitting a datagram, not slice out of bounds.
fn respond_oversized(
    _ctx: usize,
    _source: SocketAddrV4,
    _service_id: u16,
    _method_id: u16,
    _payload: &[u8],
    _e2e_status: u8,
    response_out: &mut [u8],
) -> i32 {
    (response_out.len() + 100) as i32
}

/// A non-negative responder return frames a SOME/IP RESPONSE — message
/// type 0x80, request id echoed, the written body as payload — and sends
/// it back to the requester.
#[tokio::test]
async fn non_sd_responder_frames_and_sends_response() {
    let unicast_pipe = Arc::new(MockPipe::default());
    let factory = MockFactory {
        unicast_pipe: Arc::clone(&unicast_pipe),
        sd_pipe: Arc::new(MockPipe::default()),
        next_port: Arc::new(Mutex::new(0)),
    };

    let e2e_handle: Arc<Mutex<E2ERegistry>> = Arc::new(Mutex::new(E2ERegistry::new()));
    let config = ServerConfig::new(0x1234, 1)
        .with_interface(Ipv4Addr::LOCALHOST)
        .with_local_port(30702);

    let deps: ServerDeps<MockFactory, MockTimer, Arc<Mutex<E2ERegistry>>, MockSubscriptions> =
        ServerDeps {
            factory,
            timer: MockTimer,
            e2e_registry: e2e_handle,
            subscriptions: MockSubscriptions::default(),
            non_sd_observer: Some((respond_with_body as NonSdRequestCallback, 0)),
        };

    let (_server, _handles, run): (
        Server<MockFactory, MockTimer, Arc<Mutex<E2ERegistry>>, MockSubscriptions>,
        _,
        _,
    ) = Server::new_with_deps(deps, config, false)
        .await
        .expect("Server::new_with_deps must succeed");
    let handle = tokio::spawn(run);

    let src = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 110), 40010);
    let request_id = 0xABCD_1234u32;
    unicast_pipe.inbound.lock().unwrap().push_back((
        build_method_request_with_id(0x1234, 0x0001, request_id, &[0xDE, 0xAD]),
        src,
    ));
    if let Some(w) = unicast_pipe.inbound_waker.lock().unwrap().take() {
        w.wake();
    }

    drive_until(|| !unicast_pipe.sent.lock().unwrap().is_empty()).await;

    let (resp, target) = unicast_pipe
        .sent
        .lock()
        .unwrap()
        .pop_front()
        .expect("a RESPONSE datagram must be sent");
    assert_eq!(target, src, "response goes back to the requester");
    assert!(resp.len() >= 16, "response has a full SOME/IP header");
    assert_eq!(&resp[0..2], &0x1234u16.to_be_bytes(), "service id");
    assert_eq!(&resp[2..4], &0x0001u16.to_be_bytes(), "method id");
    assert_eq!(
        &resp[4..8],
        &(8u32 + 3).to_be_bytes(),
        "length = 8 (upper header) + 3-byte body"
    );
    assert_eq!(
        &resp[8..12],
        &request_id.to_be_bytes(),
        "request id echoed verbatim"
    );
    assert_eq!(resp[14], 0x80, "message type = Response");
    assert_eq!(&resp[16..], &[0x11, 0x22, 0x33], "body the callback wrote");

    handle.abort();
    let _ = handle.await;
}

/// A responder that returns a length larger than the buffer it was given
/// (a contract violation, but the callback is consumer/FFI code) must be
/// rejected: no datagram sent, run-future still alive — never an
/// out-of-bounds slice panic.
#[tokio::test]
async fn non_sd_responder_oversized_length_is_rejected_not_panicked() {
    let unicast_pipe = Arc::new(MockPipe::default());
    let factory = MockFactory {
        unicast_pipe: Arc::clone(&unicast_pipe),
        sd_pipe: Arc::new(MockPipe::default()),
        next_port: Arc::new(Mutex::new(0)),
    };

    let e2e_handle: Arc<Mutex<E2ERegistry>> = Arc::new(Mutex::new(E2ERegistry::new()));
    let config = ServerConfig::new(0x1234, 1)
        .with_interface(Ipv4Addr::LOCALHOST)
        .with_local_port(30703);

    let deps: ServerDeps<MockFactory, MockTimer, Arc<Mutex<E2ERegistry>>, MockSubscriptions> =
        ServerDeps {
            factory,
            timer: MockTimer,
            e2e_registry: e2e_handle,
            subscriptions: MockSubscriptions::default(),
            non_sd_observer: Some((respond_oversized as NonSdRequestCallback, 0)),
        };

    let (_server, _handles, run): (
        Server<MockFactory, MockTimer, Arc<Mutex<E2ERegistry>>, MockSubscriptions>,
        _,
        _,
    ) = Server::new_with_deps(deps, config, false)
        .await
        .expect("Server::new_with_deps must succeed");
    let handle = tokio::spawn(run);

    let src = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 111), 40011);
    unicast_pipe
        .inbound
        .lock()
        .unwrap()
        .push_back((build_method_request(0x1234, 0x0001, &[]), src));
    if let Some(w) = unicast_pipe.inbound_waker.lock().unwrap().take() {
        w.wake();
    }

    // Drive until the request is consumed, then yield once more to let
    // any (buggy) response attempt happen.
    drive_until(|| unicast_pipe.inbound.lock().unwrap().is_empty()).await;
    tokio::task::yield_now().await;

    assert!(
        unicast_pipe.sent.lock().unwrap().is_empty(),
        "an over-range response length must be dropped, not framed/sent"
    );
    assert!(
        !handle.is_finished(),
        "run-future must survive a contract-violating response length \
         (no out-of-bounds slice panic)"
    );
    handle.abort();
    let _ = handle.await;
}

// ── Co-offered SubscribeEventgroup acceptance ─────────────────────────
//
// A service registered via `with_accepted_offer` is accepted on the
// shared recv loop even though it is not the primary service. The
// accepted-offer tuple includes the major version, so a Subscribe whose
// major version does not match the registered co-offer must be rejected
// — the same contract the primary service enforces — not silently
// accepted by skipping the version guard for co-offered tuples.

/// Drive one `SubscribeEventgroup` for co-offered service `0x5678`
/// (registered with major version 2) carrying `subscribe_major`, and
/// return whatever subscriptions the server recorded.
async fn drive_co_offer_subscribe(local_port: u16, subscribe_major: u8) -> Vec<SubKey> {
    use simple_someip::protocol::sd::RebootFlag;
    use simple_someip::sd_codec::{build_subscribe_eventgroup_datagram, SubscribeEventgroupRequest};

    let unicast_pipe = Arc::new(MockPipe::default());
    let sd_pipe = Arc::new(MockPipe::default());
    let factory = MockFactory {
        unicast_pipe: Arc::clone(&unicast_pipe),
        sd_pipe: Arc::clone(&sd_pipe),
        next_port: Arc::new(Mutex::new(0)),
    };
    let e2e_handle: Arc<Mutex<E2ERegistry>> = Arc::new(Mutex::new(E2ERegistry::new()));
    let subs_log: Arc<Mutex<Vec<SubKey>>> = Arc::new(Mutex::new(Vec::new()));
    let subs = MockSubscriptions(subs_log.clone());

    // Primary service 0x1234 (major 1); co-offer service 0x5678 at major 2.
    let config = ServerConfig::new(0x1234, 1)
        .with_interface(Ipv4Addr::LOCALHOST)
        .with_local_port(local_port)
        .with_accepted_offer(0x5678, 1, 2, 0x0001);

    let deps: ServerDeps<MockFactory, MockTimer, Arc<Mutex<E2ERegistry>>, MockSubscriptions> =
        ServerDeps {
            factory,
            timer: MockTimer,
            e2e_registry: e2e_handle,
            subscriptions: subs,
            non_sd_observer: None,
        };
    let (_server, _handles, run): (
        Server<MockFactory, MockTimer, Arc<Mutex<E2ERegistry>>, MockSubscriptions>,
        _,
        _,
    ) = Server::new_with_deps(deps, config, false)
        .await
        .expect("Server::new_with_deps must succeed");
    let handle = tokio::spawn(run);

    let req = SubscribeEventgroupRequest {
        service_id: 0x5678,
        instance_id: 1,
        major_version: subscribe_major,
        event_group_id: 0x0001,
        ttl: 0x00FF_FFFF,
        local_ip: Ipv4Addr::new(192, 0, 2, 200),
        local_rx_port: 45_000,
    };
    let mut buf = [0u8; 256];
    let n = build_subscribe_eventgroup_datagram(&mut buf, &req, 1, RebootFlag::RecentlyRebooted)
        .expect("encode subscribe");
    let src = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 200), 45_000);
    sd_pipe
        .inbound
        .lock()
        .unwrap()
        .push_back((buf[..n].to_vec(), src));
    if let Some(w) = sd_pipe.inbound_waker.lock().unwrap().take() {
        w.wake();
    }

    // Drive until the SD datagram is consumed, then yield enough times for
    // the subscribe future (and any Ack/Nack send) to settle.
    drive_until(|| sd_pipe.inbound.lock().unwrap().is_empty()).await;
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }

    handle.abort();
    let _ = handle.await;
    let recorded = subs_log.lock().unwrap().clone();
    recorded
}

/// A co-offered Subscribe whose major version matches the registered
/// co-offer is accepted.
#[tokio::test]
async fn co_offered_subscribe_with_matching_major_version_is_accepted() {
    let recorded = drive_co_offer_subscribe(30710, 2).await;
    assert_eq!(
        recorded.len(),
        1,
        "co-offered subscribe with the registered major version must be accepted"
    );
    assert_eq!(
        recorded[0].0, 0x5678,
        "subscription recorded for the co-offered service"
    );
}

/// A co-offered Subscribe whose major version does NOT match the
/// registered co-offer must be rejected — the version guard applies to
/// co-offered tuples, not only the primary service.
#[tokio::test]
async fn co_offered_subscribe_with_wrong_major_version_is_rejected() {
    let recorded = drive_co_offer_subscribe(30711, 99).await;
    assert!(
        recorded.is_empty(),
        "co-offered subscribe with a mismatched major version must be rejected, \
         not silently accepted by skipping the version guard"
    );
}
