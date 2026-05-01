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
    let pipe = Arc::new(MockPipe::default());
    let factory = MockFactory {
        pipe: Arc::clone(&pipe),
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
        };

    let (_server, _handles, run): (
        Server<MockFactory, MockTimer, Arc<Mutex<E2ERegistry>>, MockSubscriptions>,
        _,
        _,
    ) = Server::new_with_deps(deps, config, false)
        .await
        .expect("Server::new_with_deps must succeed with no-tokio mocks");

    // Phase 21b: receive + announce folded into the combined run-future.
    // Spawning it on tokio proves it's `'static`. The witness is purely
    // structural: if this line compiles, `Server` is reachable on a
    // no-tokio build.
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
    let pipe = Arc::new(MockPipe::default());
    let factory = MockFactory {
        pipe: Arc::clone(&pipe),
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
        };

    let (_server, _handles, _run): (
        Server<MockFactory, MockTimer, Arc<Mutex<E2ERegistry>>, MockSubscriptions>,
        _,
        _,
    ) = Server::new_passive_with_deps(deps, config)
        .await
        .expect("Server::new_passive_with_deps must succeed with no-tokio mocks");
}
