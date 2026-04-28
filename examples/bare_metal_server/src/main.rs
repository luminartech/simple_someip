//! Host-side demonstration of [`Server::new_with_deps`] on a no-tokio,
//! no-socket2 build.
//!
//! # What this example shows
//!
//! `simple-someip` is compiled with
//! `default-features = false, features = ["server", "bare_metal"]` —
//! no tokio, no socket2 pulled in by *the crate itself*. The example
//! binary adds tokio only for its own executor and mock driver; real
//! firmware would use `embassy_executor` (or any bare-metal async
//! runtime) instead.
//!
//! Building or running this example in isolation proves that the
//! bare-metal server API compiles under exactly the feature set a
//! firmware consumer would use:
//!
//! ```text
//! cargo build -p bare_metal_server
//! cargo run  -p bare_metal_server
//! ```
//!
//! # Patterns demonstrated
//!
//! | Pattern | This example | Firmware replacement |
//! |---------|-------------|----------------------|
//! | Transport | `MockFactory` / `MockSocket` | `embassy_net`, smoltcp, custom Ethernet ISR |
//! | Timer | `MockTimer` using `tokio::task::yield_now` | `embassy_time::Timer::after` |
//! | Subscription table | `MockSubscriptions` | `heapless`-backed table behind a CS mutex |
//! | Lock handle | `Arc<Mutex<E2ERegistry>>` | stack-allocated handle (see below) |
//!
//! # What is not yet demonstrated
//!
//! The `E2ERegistry` handle still uses a heap-allocated `Arc<Mutex<_>>`.
//! A future verification pass will replace this with a stack-allocated
//! alternative and confirm zero heap allocation after
//! `Server::new_with_deps` returns.
//!
//! [`Server::new_with_deps`]: simple_someip::Server::new_with_deps

use core::future::Future;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::pin::Pin;
use core::task::{Context, Poll};
use core::time::Duration;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::vec::Vec;

use simple_someip::e2e::E2ERegistry;
use simple_someip::server::{ServerConfig, SubscribeError, Subscriber, SubscriptionHandle};
use simple_someip::transport::{
    ReceivedDatagram, SocketOptions, Timer, TransportError, TransportFactory, TransportSocket,
};
use simple_someip::{Server, ServerDeps};

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
            40000u16.saturating_add(*p)
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

#[derive(Clone)]
struct MockTimer;

impl Timer for MockTimer {
    async fn sleep(&self, _duration: Duration) {
        tokio::task::yield_now().await;
    }
}

// ── Mock SubscriptionHandle ───────────────────────────────────────────
//
// On `server-tokio`, `Arc<RwLock<SubscriptionManager>>` is the built-in
// impl. Bare-metal callers supply their own. A real firmware impl would
// back this with a `critical_section::Mutex<RefCell<_>>` or
// `spin::Mutex<_>` over a `heapless`-backed table; here we use
// `std::sync::Mutex` over a `Vec` because the example runs on the host.
// The trait impl itself is the portable pattern — only the concurrency
// primitive and storage type change on firmware.

type SubKey = (u16, u16, u16, SocketAddrV4);

#[derive(Clone, Default)]
struct MockSubscriptions(Arc<Mutex<Vec<SubKey>>>);

impl SubscriptionHandle for MockSubscriptions {
    fn subscribe(
        &self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
        subscriber_addr: SocketAddrV4,
    ) -> impl Future<Output = Result<(), SubscribeError>> + Send + '_ {
        let inner = Arc::clone(&self.0);
        async move {
            let mut guard = inner.lock().unwrap();
            let key = (service_id, instance_id, event_group_id, subscriber_addr);
            if !guard.contains(&key) {
                guard.push(key);
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
    ) -> impl Future<Output = ()> + Send + '_ {
        let inner = Arc::clone(&self.0);
        async move {
            inner
                .lock()
                .unwrap()
                .retain(|e| *e != (service_id, instance_id, event_group_id, subscriber_addr));
        }
    }

    fn get_subscribers(
        &self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
    ) -> impl Future<Output = Vec<Subscriber>> + Send + '_ {
        let inner = Arc::clone(&self.0);
        async move {
            inner
                .lock()
                .unwrap()
                .iter()
                .filter(|(s, i, e, _)| {
                    *s == service_id && *i == instance_id && *e == event_group_id
                })
                .map(|(s, i, e, addr)| Subscriber::new(*addr, *s, *i, *e))
                .collect()
        }
    }
}

// ── Main ──────────────────────────────────────────────────────────────

// current_thread matches a single-core bare-metal executor; yields are
// fully sequential, which lets the assertion below observe the first
// SD announcement reliably.
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let pipe = Arc::new(MockPipe::default());
    let factory = MockFactory {
        pipe: Arc::clone(&pipe),
        next_port: Arc::new(Mutex::new(0)),
    };

    // std Arc/Mutex implements E2ERegistryHandle and is gated by
    // `feature = "std"`, not `server-tokio`. A future no-alloc port
    // replaces this with a stack-allocated handle.
    let e2e: Arc<Mutex<E2ERegistry>> = Arc::new(Mutex::new(E2ERegistry::new()));
    let subs = MockSubscriptions::default();

    // service_id=0x1234, instance_id=1, bound to LOCALHOST:30490.
    let config = ServerConfig::new(Ipv4Addr::LOCALHOST, 30490, 0x1234, 1);

    let server = Server::<
        Arc<Mutex<E2ERegistry>>,
        MockSubscriptions,
        MockFactory,
        MockTimer,
    >::new_with_deps(
        ServerDeps { factory, timer: MockTimer, e2e_registry: e2e, subscriptions: subs },
        config,
        false, // multicast_loopback
    )
    .await
    .expect("Server::new_with_deps failed");

    // The announcement loop periodically multicasts SD OfferService
    // entries so clients on the network can discover this service.
    // It is Send + 'static and can be handed to any executor.
    let announce_handle = tokio::spawn(
        server.announcement_loop().expect("non-passive server must have an announcement loop"),
    );

    // Yield twice: the announcement loop fires its first SD offer on the
    // first poll before the inter-announcement timer starts.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // Verify the server actually sent at least one SD announcement.
    let sent = pipe.sent.lock().unwrap().len();
    assert!(sent > 0, "server should have multicast at least one SD OfferService");

    announce_handle.abort();
    let _ = announce_handle.await;

    println!(
        "bare-metal server example: Server::new_with_deps compiled and ran successfully \
         under features=[server, bare_metal] — no tokio / socket2 from simple-someip itself. \
         SD announcements sent: {sent}."
    );
}
