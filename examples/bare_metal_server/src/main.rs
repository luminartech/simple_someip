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
//! | Timer | `MockTimer` using `tokio::time::sleep` | `embassy_time::Timer::after` |
//! | Subscription table | `StaticSubscriptionHandle` over `&'static StaticSubscriptionStorage` | same — already firmware-ready |
//! | E2E registry | `StaticE2EHandle` over `&'static StaticE2EStorage` | same — already firmware-ready |
//!
//! Both handles are pure `no_std` (no allocator required) and use a
//! `&'static` critical-section mutex around the underlying state, which
//! is the firmware-target shape. `E2ERegistry::new()` and
//! `SubscriptionManager::new()` are both `const`, so the storage lives
//! in plain `static` declarations at module scope (see `E2E_STORAGE`
//! and `SUBS_STORAGE` near the top of this file).
//!
//! [`Server::new_with_deps`]: simple_someip::Server::new_with_deps

use core::cell::RefCell;
use core::future::Future;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::pin::Pin;
use core::task::{Context, Poll};
use core::time::Duration;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::vec::Vec;

use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use simple_someip::e2e::E2ERegistry;
use simple_someip::server::{
    ServerConfig, StaticSubscriptionHandle, StaticSubscriptionStorage, SubscriptionManager,
};
use simple_someip::transport::{
    ReceivedDatagram, SocketOptions, Timer, TransportError, TransportFactory, TransportSocket,
};
use simple_someip::{Server, ServerDeps, StaticE2EHandle, StaticE2EStorage};

// ── Bare-metal lock-handle storage ────────────────────────────────────
//
// `&'static` storage for the no-alloc lock handles. Both
// `E2ERegistry::new()` and `SubscriptionManager::new()` are `const`,
// so the storage lives in plain `static`s — no `Box::leak` required.
// On real firmware you'd write the same `static` declarations in
// boot code.

static E2E_STORAGE: StaticE2EStorage =
    BlockingMutex::<CriticalSectionRawMutex, RefCell<E2ERegistry>>::new(RefCell::new(
        E2ERegistry::new(),
    ));

static SUBS_STORAGE: StaticSubscriptionStorage = BlockingMutex::<
    CriticalSectionRawMutex,
    RefCell<SubscriptionManager>,
>::new(RefCell::new(SubscriptionManager::new()));

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
            40000u16.saturating_add(*p)
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
// Honors `duration` per the `Timer` trait contract. Real
// firmware replaces this with e.g. `embassy_time::Timer::after(d).await`.

#[derive(Clone)]
struct MockTimer;

impl Timer for MockTimer {
    type SleepFuture<'a> = core::pin::Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
    fn sleep(&self, duration: Duration) -> Self::SleepFuture<'_> {
        Box::pin(async move {
            tokio::time::sleep(duration).await;
        })
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

    // Bare-metal lock handles: both StaticE2EHandle and
    // StaticSubscriptionHandle are pure no_std (alloc-free) and back
    // their state with a `&'static` critical-section mutex. The
    // `static` storages themselves live at module scope (see top of
    // file) — clippy::pedantic dislikes `static` after `let`.
    let e2e = StaticE2EHandle::new(&E2E_STORAGE);
    let subs = StaticSubscriptionHandle::new(&SUBS_STORAGE);

    // service_id=0x1234, instance_id=1, bound to LOCALHOST:30490.
    let config = ServerConfig::new(Ipv4Addr::LOCALHOST, 30490, 0x1234, 1);

    let server =
        Server::<StaticE2EHandle, StaticSubscriptionHandle, MockFactory, MockTimer>::new_with_deps(
            ServerDeps {
                factory,
                timer: MockTimer,
                e2e_registry: e2e,
                subscriptions: subs,
            },
            config,
            false, // multicast_loopback
        )
        .await
        .expect("Server::new_with_deps failed");

    // The announcement loop periodically multicasts SD OfferService
    // entries so clients on the network can discover this service.
    // It is Send + 'static and can be handed to any executor.
    let announce_handle = tokio::spawn(
        server
            .announcement_loop()
            .expect("non-passive server must have an announcement loop"),
    );

    // Yield twice: the announcement loop fires its first SD offer on the
    // first poll before the inter-announcement timer starts.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // Verify the server actually sent at least one SD announcement.
    let sent = pipe.sent.lock().unwrap().len();
    assert!(
        sent > 0,
        "server should have multicast at least one SD OfferService"
    );

    announce_handle.abort();
    let _ = announce_handle.await;

    println!(
        "bare-metal server example: Server::new_with_deps compiled and ran successfully \
         under features=[server, bare_metal] — no tokio / socket2 from simple-someip itself. \
         SD announcements sent: {sent}."
    );
}
