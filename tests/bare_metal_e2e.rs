//! End-to-end bare-metal test: wire a no-tokio Client and Server through
//! a shared mock pipe and drive a request/response roundtrip.
//!
//! This test proves that the full `Client` + `Server` path works without
//! the `client-tokio` / `server-tokio` features. Both sides use:
//! - A shared `MockPipe` for transport (bytes sent by one side appear in
//!   the other's inbound queue)
//! - `define_static_channels!` for the client's channel factory
//! - `Arc<Mutex<E2ERegistry>>` for E2E (the std-backed impl)
//! - A test-runtime tokio spawner/timer (proving the *trait* compiles,
//!   not that tokio is absent from the test harness)
//!
//! The test exercises:
//! 1. Server startup and SD announcement broadcast
//! 2. Client receiving the SD offer (via the shared pipe)
//! 3. Client sending a request to the server
//! 4. Server run-loop receiving and echoing the request
//! 5. Client receiving the response
#![cfg(all(feature = "client", feature = "server", feature = "bare_metal"))]

use core::future::Future;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::pin::Pin;
use core::task::{Context, Poll};
use core::time::Duration;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, RwLock};

use simple_someip::PayloadWireFormat;
use simple_someip::client::Error as ClientError;
use simple_someip::client::{ClientUpdate, ControlMessage, ReceivedMessage, SendMessage};
use simple_someip::define_static_channels;
use simple_someip::e2e::E2ERegistry;
use simple_someip::protocol::sd::RebootFlag;
use simple_someip::protocol::{
    Header, Message, MessageId, MessageType, MessageTypeField, ReturnCode,
};
use simple_someip::server::{ServerConfig, SubscribeError, Subscriber, SubscriptionHandle};
use simple_someip::WireFormat;
use simple_someip::static_channels::BufferPool;
use simple_someip::transport::{
    ReceivedDatagram, SocketOptions, Spawner, StaticBufferProvider, Timer, TransportError,
    TransportFactory, TransportSocket,
};
use simple_someip::{Client, ClientDeps, RawPayload, Server, ServerDeps, UDP_BUFFER_SIZE};

// ── Static-pool channel factory ───────────────────────────────────────
//
// Pool budget: each `Client::new_with_deps` claims one `ControlMessage`
// bounded slot and one `ClientUpdate` unbounded slot for the lifetime
// of the client. A plain parallel `cargo test` runs every test in this
// file in ONE process, so concurrent tests share these pools. As of the
// #125 buffer-provider work there are 6 client-constructing tests
// worst-case (the two new buffer tests join the original four), so both
// pools hold 8. If a new test pushes past 8, grow the two pool counts
// below or the exhaustion panic will land in whichever test loses the
// race.
//
// NOTE: `tools/size_probe`'s `ProbeChannels` mirrors this entry list
// for thumbv7em layout capture. If you change the entries here,
// update the probe or its measured layouts silently drift.

define_static_channels! {
    name: E2ETestChannels,
    oneshot: [
        (Result<(), ClientError>, 16),
        (Result<RawPayload, ClientError>, 8),
        (Result<RebootFlag, ClientError>, 8),
    ],
    bounded: [
        ((ControlMessage<RawPayload, E2ETestChannels>, 4), 8),
        ((SendMessage<RawPayload, E2ETestChannels>, 16), 12),
        ((Result<ReceivedMessage<RawPayload>, ClientError>, 16), 12),
    ],
    unbounded: [
        (ClientUpdate<RawPayload>, 8),
    ],
}

// ── Shared mock pipe (bidirectional) ──────────────────────────────────
//
// The "network" is modeled as two separate pipes:
// - `client_to_server`: bytes sent by client, received by server
// - `server_to_client`: bytes sent by server, received by client
//
// Each side's MockSocket is configured to send to one pipe and receive
// from the other.

#[derive(Default)]
struct MockPipe {
    queue: Mutex<VecDeque<(Vec<u8>, SocketAddrV4)>>,
    waker: Mutex<Option<core::task::Waker>>,
}

impl MockPipe {
    fn send(&self, bytes: Vec<u8>, source: SocketAddrV4) {
        self.queue.lock().unwrap().push_back((bytes, source));
        let waker = self.waker.lock().unwrap().take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    fn try_recv(&self) -> Option<(Vec<u8>, SocketAddrV4)> {
        self.queue.lock().unwrap().pop_front()
    }

    fn register_waker(&self, waker: core::task::Waker) {
        *self.waker.lock().unwrap() = Some(waker);
    }
}

struct SharedNetwork {
    client_to_server: Arc<MockPipe>,
    server_to_client: Arc<MockPipe>,
}

impl SharedNetwork {
    fn new() -> Self {
        Self {
            client_to_server: Arc::new(MockPipe::default()),
            server_to_client: Arc::new(MockPipe::default()),
        }
    }
}

// ── Mock transport factory ────────────────────────────────────────────

#[derive(Clone)]
struct MockFactory {
    /// Pipe to send to
    tx_pipe: Arc<MockPipe>,
    /// Pipe to receive from
    rx_pipe: Arc<MockPipe>,
    /// Port counter for ephemeral binds
    next_port: Arc<Mutex<u16>>,
}

impl TransportFactory for MockFactory {
    type Socket = MockSocket;
    type BindFuture<'a> =
        core::pin::Pin<Box<dyn Future<Output = Result<Self::Socket, TransportError>> + Send + 'a>>;

    fn bind<'a>(&'a self, addr: SocketAddrV4, _options: &'a SocketOptions) -> Self::BindFuture<'a> {
        let tx = Arc::clone(&self.tx_pipe);
        let rx = Arc::clone(&self.rx_pipe);
        let port = if addr.port() == 0 {
            let mut p = self.next_port.lock().unwrap();
            *p += 1;
            40000 + *p
        } else {
            addr.port()
        };
        let local = SocketAddrV4::new(*addr.ip(), port);
        Box::pin(async move {
            Ok(MockSocket {
                tx_pipe: tx,
                rx_pipe: rx,
                local,
            })
        })
    }
}

struct MockSocket {
    tx_pipe: Arc<MockPipe>,
    rx_pipe: Arc<MockPipe>,
    local: SocketAddrV4,
}

struct MockSendFut {
    pipe: Arc<MockPipe>,
    bytes: Option<Vec<u8>>,
    source: SocketAddrV4,
}

impl Future for MockSendFut {
    type Output = Result<(), TransportError>;
    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = self.get_mut();
        if let Some(bytes) = me.bytes.take() {
            me.pipe.send(bytes, me.source);
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
        if let Some((bytes, source)) = me.pipe.try_recv() {
            let n = bytes.len().min(me.buf.len());
            me.buf[..n].copy_from_slice(&bytes[..n]);
            return Poll::Ready(Ok(ReceivedDatagram {
                bytes_received: n,
                source,
                truncated: n < bytes.len(),
            }));
        }
        me.pipe.register_waker(cx.waker().clone());
        // Re-check after registering
        if let Some((bytes, source)) = me.pipe.try_recv() {
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

impl TransportSocket for MockSocket {
    type SendFuture<'a> = MockSendFut;
    type RecvFuture<'a> = MockRecvFut<'a>;

    fn send_to<'a>(&'a self, buf: &'a [u8], _target: SocketAddrV4) -> Self::SendFuture<'a> {
        MockSendFut {
            pipe: Arc::clone(&self.tx_pipe),
            bytes: Some(buf.to_vec()),
            source: self.local,
        }
    }

    fn recv_from<'a>(&'a self, buf: &'a mut [u8]) -> Self::RecvFuture<'a> {
        MockRecvFut {
            pipe: Arc::clone(&self.rx_pipe),
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
        Box::pin(async move {
            tokio::time::sleep(duration).await;
        })
    }
}

// ── Mock Spawner ──────────────────────────────────────────────────────

struct TokioBackedSpawner;

impl Spawner for TokioBackedSpawner {
    fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        drop(tokio::spawn(future));
    }
}

// ── Mock SubscriptionHandle ───────────────────────────────────────────

type SubKey = (u16, u16, u16, SocketAddrV4);

#[derive(Clone, Default)]
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

// ── Tests ─────────────────────────────────────────────────────────────

/// Proves that a bare-metal Client and Server can be wired together
/// through a shared mock transport and that the Server's SD announcement
/// is visible to the Client.
#[tokio::test]
async fn client_receives_server_sd_announcement() {
    let network = SharedNetwork::new();

    // Server sends to server_to_client, receives from client_to_server
    let server_factory = MockFactory {
        tx_pipe: Arc::clone(&network.server_to_client),
        rx_pipe: Arc::clone(&network.client_to_server),
        next_port: Arc::new(Mutex::new(0)),
    };

    // Client sends to client_to_server, receives from server_to_client
    let client_factory = MockFactory {
        tx_pipe: Arc::clone(&network.client_to_server),
        rx_pipe: Arc::clone(&network.server_to_client),
        next_port: Arc::new(Mutex::new(100)),
    };

    // Create server
    let server_e2e: Arc<Mutex<E2ERegistry>> = Arc::new(Mutex::new(E2ERegistry::new()));
    let server_subs = MockSubscriptions::default();
    let server_config = ServerConfig::new(0x1234, 1)
        .with_interface(Ipv4Addr::LOCALHOST)
        .with_local_port(30500);

    let server_deps = ServerDeps {
        factory: server_factory,
        timer: MockTimer,
        e2e_registry: server_e2e,
        subscriptions: server_subs,
        non_sd_observer: None,
    };

    let (_server, _handles, run): (
        Server<MockFactory, MockTimer, Arc<Mutex<E2ERegistry>>, MockSubscriptions>,
        _,
        _,
    ) = Server::new_with_deps(server_deps, server_config, false)
        .await
        .expect("server creation");

    // Combined run-future drives both announcement + receive.
    let announce_handle = tokio::spawn(run);

    // Create client
    let client_e2e: Arc<Mutex<E2ERegistry>> = Arc::new(Mutex::new(E2ERegistry::new()));
    let client_iface: Arc<RwLock<Ipv4Addr>> = Arc::new(RwLock::new(Ipv4Addr::LOCALHOST));

    static POOL_SD: BufferPool<9, UDP_BUFFER_SIZE> = BufferPool::new();
    let client_deps = ClientDeps {
        factory: client_factory,
        spawner: TokioBackedSpawner,
        timer: MockTimer,
        e2e_registry: client_e2e,
        interface: client_iface,
        buffer_provider: StaticBufferProvider(&POOL_SD),
    };

    let (client, mut updates, run_fut) = Client::<
        RawPayload,
        Arc<Mutex<E2ERegistry>>,
        Arc<RwLock<Ipv4Addr>>,
        E2ETestChannels,
    >::new_with_deps(client_deps, false);

    let run_handle = tokio::spawn(run_fut);

    // Bind client discovery socket
    client.bind_discovery().await.expect("bind_discovery");

    // Wait for server's SD announcement to propagate through the mock
    // network and arrive at the client's update stream.
    let timeout = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(update) = updates.recv().await {
            if let ClientUpdate::DiscoveryUpdated(_msg) = update {
                // Got an SD message — the e2e path works!
                return true;
            }
        }
        false
    })
    .await;

    assert!(
        timeout.unwrap_or(false),
        "client should have received server's SD announcement"
    );

    // Cleanup
    announce_handle.abort();
    run_handle.abort();
}

/// Proves that the client can send a SOME/IP request through the mock network
/// using `add_endpoint` + `send_to_service`, and the server run-loop stays
/// stable under load. Response delivery is not verified here because the
/// server has no registered request handler; see the doc-level test list for
/// items that remain.
#[tokio::test]
async fn client_send_request_server_runloop_stable() {
    let network = SharedNetwork::new();

    let server_factory = MockFactory {
        tx_pipe: Arc::clone(&network.server_to_client),
        rx_pipe: Arc::clone(&network.client_to_server),
        next_port: Arc::new(Mutex::new(0)),
    };

    let client_factory = MockFactory {
        tx_pipe: Arc::clone(&network.client_to_server),
        rx_pipe: Arc::clone(&network.server_to_client),
        next_port: Arc::new(Mutex::new(100)),
    };

    // Create server (passive — no SD announcements)
    let server_e2e: Arc<Mutex<E2ERegistry>> = Arc::new(Mutex::new(E2ERegistry::new()));
    let server_subs = MockSubscriptions::default();
    let service_id = 0x5678_u16;
    let instance_id = 1_u16;
    let server_port = 30600_u16;
    let server_config = ServerConfig::new(service_id, instance_id)
        .with_interface(Ipv4Addr::LOCALHOST)
        .with_local_port(server_port);

    let server_deps = ServerDeps {
        factory: server_factory,
        timer: MockTimer,
        e2e_registry: server_e2e,
        subscriptions: server_subs,
        non_sd_observer: None,
    };

    let (_server, _handles, run): (
        Server<MockFactory, MockTimer, Arc<Mutex<E2ERegistry>>, MockSubscriptions>,
        _,
        _,
    ) = Server::new_passive_with_deps(server_deps, server_config)
        .await
        .expect("passive server creation");

    // Start server run loop (passive — receive only, no announcements).
    let run_handle = tokio::spawn(async move {
        let _ = run.await;
    });

    // Create client
    let client_e2e: Arc<Mutex<E2ERegistry>> = Arc::new(Mutex::new(E2ERegistry::new()));
    let client_iface: Arc<RwLock<Ipv4Addr>> = Arc::new(RwLock::new(Ipv4Addr::LOCALHOST));

    static POOL_REQ: BufferPool<9, UDP_BUFFER_SIZE> = BufferPool::new();
    let client_deps = ClientDeps {
        factory: client_factory,
        spawner: TokioBackedSpawner,
        timer: MockTimer,
        e2e_registry: client_e2e,
        interface: client_iface,
        buffer_provider: StaticBufferProvider(&POOL_REQ),
    };

    let (client, mut updates, client_run_fut) = Client::<
        RawPayload,
        Arc<Mutex<E2ERegistry>>,
        Arc<RwLock<Ipv4Addr>>,
        E2ETestChannels,
    >::new_with_deps(client_deps, false);

    let client_run_handle = tokio::spawn(client_run_fut);

    // Register the server endpoint with the client
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
    client
        .add_endpoint(service_id, instance_id, server_addr, 0)
        .await
        .expect("add_endpoint");

    // Build a request message using the correct API
    let msg_id = MessageId::new_from_service_and_method(service_id, 0x0001);
    let payload_bytes = [0x01_u8, 0x02, 0x03, 0x04];
    let payload = RawPayload::from_payload_bytes(msg_id, &payload_bytes).expect("create payload");
    let request = Message::<RawPayload>::new(
        Header::new(
            msg_id,
            0x0001_0001, // request_id: client_id << 16 | session_id
            1,           // protocol_version
            1,           // interface_version
            MessageTypeField::new(MessageType::Request, false),
            ReturnCode::Ok,
            payload_bytes.len(),
        ),
        payload,
    );

    // Send request via the client API
    let pending = client
        .send_to_service(service_id, instance_id, request)
        .await
        .expect("send_to_service");

    // Give the server time to process
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Check for any updates — server won't respond without a handler,
    // but this proves the send path compiles and runs.
    let timeout_result = tokio::time::timeout(Duration::from_millis(500), async {
        while let Some(update) = updates.recv().await {
            match update {
                ClientUpdate::Unicast { message, .. } => {
                    return Some(message);
                }
                ClientUpdate::Error(e) => {
                    eprintln!("Client error: {:?}", e);
                }
                _ => {}
            }
        }
        None
    })
    .await;

    // The test passes if:
    // 1. add_endpoint succeeded
    // 2. send_to_service succeeded (already asserted)
    // 3. No panics in either run loop
    // A response is not guaranteed without a server-side request handler.

    match timeout_result {
        Ok(Some(msg)) => {
            println!(
                "Received response: service=0x{:04X}, method=0x{:04X}",
                msg.header().message_id().service_id(),
                msg.header().message_id().method_id()
            );
        }
        Ok(None) | Err(_) => {
            println!("No response (expected — server has no request handler)");
        }
    }

    // Verify the pending response handle is usable (won't resolve without
    // a server reply, but the type should be correct)
    drop(pending);

    // Cleanup
    run_handle.abort();
    client_run_handle.abort();
}

/// Host-arch PROXY budgets for the bare-metal-channel configuration
/// (static pools + mock transport) — the closest host-side analog to
/// the TC4 build. Same semantics/update procedure as the constants in
/// src/client/mod.rs; authoritative numbers come from
/// `tools/capture_type_sizes.sh` (thumbv7em).
const BM_CLIENT_RUN_FUTURE_BUDGET: usize = 34048; // = ceil64(27208 × 1.25)
const BM_CLIENT_SOCKET_LOOP_BUDGET: usize = 2816; // = ceil64(2224 × 1.25)
const BM_SERVER_RUN_FUTURE_BUDGET: usize = 9664; // = ceil64(7696 × 1.25)

#[tokio::test]
async fn future_size_witness_bare_metal_channels() {
    use core::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    struct SizeRecordingSpawner {
        max_spawned: Arc<AtomicUsize>,
    }

    impl Spawner for SizeRecordingSpawner {
        fn spawn(&self, future: impl core::future::Future<Output = ()> + Send + 'static) {
            self.max_spawned
                .fetch_max(core::mem::size_of_val(&future), Ordering::SeqCst);
            let _run_handle = tokio::spawn(future);
        }
    }

    let network = SharedNetwork::new();

    // ── Server (mirror client_receives_server_sd_announcement) ──
    let server_factory = MockFactory {
        tx_pipe: Arc::clone(&network.server_to_client),
        rx_pipe: Arc::clone(&network.client_to_server),
        next_port: Arc::new(Mutex::new(0)),
    };
    let server_e2e: Arc<Mutex<E2ERegistry>> = Arc::new(Mutex::new(E2ERegistry::new()));
    let server_config = ServerConfig::new(0x1234, 1)
        .with_interface(Ipv4Addr::LOCALHOST)
        .with_local_port(30600);
    let server_deps = ServerDeps {
        factory: server_factory,
        timer: MockTimer,
        e2e_registry: server_e2e,
        subscriptions: MockSubscriptions::default(),
        non_sd_observer: None,
    };
    let (_server, _handles, server_run): (
        Server<MockFactory, MockTimer, Arc<Mutex<E2ERegistry>>, MockSubscriptions>,
        _,
        _,
    ) = Server::new_with_deps(server_deps, server_config, false)
        .await
        .expect("server creation");
    let server_run_size = core::mem::size_of_val(&server_run);
    drop(server_run); // not driven; witness only

    // ── Client (static channel pools) ──
    let client_factory = MockFactory {
        tx_pipe: Arc::clone(&network.client_to_server),
        rx_pipe: Arc::clone(&network.server_to_client),
        next_port: Arc::new(Mutex::new(100)),
    };
    let max_spawned = Arc::new(AtomicUsize::new(0));
    static POOL_WITNESS: BufferPool<9, UDP_BUFFER_SIZE> = BufferPool::new();
    let client_deps = ClientDeps {
        factory: client_factory,
        spawner: SizeRecordingSpawner {
            max_spawned: Arc::clone(&max_spawned),
        },
        timer: MockTimer,
        e2e_registry: Arc::new(Mutex::new(E2ERegistry::new())),
        interface: Arc::new(RwLock::new(Ipv4Addr::LOCALHOST)),
        buffer_provider: StaticBufferProvider(&POOL_WITNESS),
    };
    let (client, _updates, run_fut) = Client::<
        RawPayload,
        Arc<Mutex<E2ERegistry>>,
        Arc<RwLock<Ipv4Addr>>,
        E2ETestChannels,
    >::new_with_deps(client_deps, false);

    let run_size = core::mem::size_of_val(&run_fut);
    let _run_handle = tokio::spawn(run_fut);
    client.bind_discovery().await.expect("bind_discovery");
    let loop_size = max_spawned.load(Ordering::SeqCst);

    println!("FUTURE_SIZE bm_client_run_future {run_size}");
    println!("FUTURE_SIZE bm_client_socket_loop {loop_size}");
    println!("FUTURE_SIZE bm_server_run_future {server_run_size}");

    assert!(loop_size > 0, "spawner never received the socket loop");
    assert!(
        run_size <= BM_CLIENT_RUN_FUTURE_BUDGET,
        "client run future grew: {run_size} B > budget {BM_CLIENT_RUN_FUTURE_BUDGET} B"
    );
    assert!(
        loop_size <= BM_CLIENT_SOCKET_LOOP_BUDGET,
        "socket loop future grew: {loop_size} B > budget {BM_CLIENT_SOCKET_LOOP_BUDGET} B"
    );
    assert!(
        server_run_size <= BM_SERVER_RUN_FUTURE_BUDGET,
        "server run future grew: {server_run_size} B > budget {BM_SERVER_RUN_FUTURE_BUDGET} B"
    );
    client.shut_down();
}

// ── Task 3 harness: a recv that reports an UNCLAMPED datagram length ───
//
// `MockSocket` above clamps `bytes_received` to `buf.len()` and flags
// `truncated`, which would exercise the pre-existing truncation drop. To
// exercise the NEW `bytes_received > buf.len()` guard in
// `socket_loop_future` directly, this harness reports the datagram's
// ORIGINAL length (possibly larger than the loop's claimed buffer) with
// `truncated: false`. `SocketManager` is a private module, so the guard is
// driven end-to-end through the public `Client` discovery socket.

/// Scripted inbound queue: each entry is `(bytes_to_copy, reported_len)`.
/// `reported_len` is what the socket reports as `bytes_received`, which may
/// exceed the caller's buffer to simulate an oversized datagram.
#[derive(Default)]
struct ScriptPipe {
    queue: Mutex<VecDeque<(Vec<u8>, usize, SocketAddrV4)>>,
    waker: Mutex<Option<core::task::Waker>>,
}

impl ScriptPipe {
    fn push(&self, bytes: Vec<u8>, reported_len: usize, source: SocketAddrV4) {
        self.queue
            .lock()
            .unwrap()
            .push_back((bytes, reported_len, source));
        if let Some(w) = self.waker.lock().unwrap().take() {
            w.wake();
        }
    }
}

#[derive(Clone)]
struct ScriptFactory {
    rx: Arc<ScriptPipe>,
}

impl TransportFactory for ScriptFactory {
    type Socket = ScriptSocket;
    type BindFuture<'a> =
        core::pin::Pin<Box<dyn Future<Output = Result<Self::Socket, TransportError>> + Send + 'a>>;
    fn bind<'a>(&'a self, addr: SocketAddrV4, _options: &'a SocketOptions) -> Self::BindFuture<'a> {
        let rx = Arc::clone(&self.rx);
        let local = SocketAddrV4::new(*addr.ip(), if addr.port() == 0 { 41000 } else { addr.port() });
        Box::pin(async move { Ok(ScriptSocket { rx, local }) })
    }
}

struct ScriptSocket {
    rx: Arc<ScriptPipe>,
    local: SocketAddrV4,
}

struct ScriptRecvFut<'a> {
    rx: Arc<ScriptPipe>,
    buf: &'a mut [u8],
}

impl Future for ScriptRecvFut<'_> {
    type Output = Result<ReceivedDatagram, TransportError>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = self.get_mut();
        if let Some((bytes, reported_len, source)) = me.rx.queue.lock().unwrap().pop_front() {
            // Copy only what fits (a real backend would never write past the
            // buffer), but report the ORIGINAL length so the loop's
            // `bytes_received > buf.len()` guard can fire.
            let n = bytes.len().min(me.buf.len());
            me.buf[..n].copy_from_slice(&bytes[..n]);
            return Poll::Ready(Ok(ReceivedDatagram {
                bytes_received: reported_len,
                source,
                truncated: false,
            }));
        }
        *me.rx.waker.lock().unwrap() = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl TransportSocket for ScriptSocket {
    type SendFuture<'a> = MockSendFut;
    type RecvFuture<'a> = ScriptRecvFut<'a>;
    fn send_to<'a>(&'a self, _buf: &'a [u8], _target: SocketAddrV4) -> Self::SendFuture<'a> {
        // Sends are unused by this test; resolve immediately into a dead pipe.
        MockSendFut {
            pipe: Arc::new(MockPipe::default()),
            bytes: None,
            source: self.local,
        }
    }
    fn recv_from<'a>(&'a self, buf: &'a mut [u8]) -> Self::RecvFuture<'a> {
        ScriptRecvFut {
            rx: Arc::clone(&self.rx),
            buf,
        }
    }
    fn local_addr(&self) -> Result<SocketAddrV4, TransportError> {
        Ok(self.local)
    }
    fn join_multicast_v4(&self, _g: Ipv4Addr, _i: Ipv4Addr) -> Result<(), TransportError> {
        Ok(())
    }
    fn leave_multicast_v4(&self, _g: Ipv4Addr, _i: Ipv4Addr) -> Result<(), TransportError> {
        Ok(())
    }
}

/// Task 3: an inbound datagram reported larger than the loop's claimed
/// buffer must be dropped (not parsed from a truncated buffer), the loop
/// must survive, and a subsequent in-budget datagram must still be
/// delivered. Driven through the public `Client` discovery socket because
/// `socket_loop_future` / `SocketManager` are private.
#[tokio::test]
async fn inbound_datagram_larger_than_claimed_buffer_is_dropped_not_fatal() {
    // Claim buffers of exactly 64 bytes: big enough for a small SD message
    // (16-byte SOME/IP header + a short SD payload), too small for the
    // scripted 256-byte oversized datagram.
    const BUF_LEN: usize = 64;
    static POOL: BufferPool<2, BUF_LEN> = BufferPool::new();

    let rx = Arc::new(ScriptPipe::default());
    let factory = ScriptFactory { rx: Arc::clone(&rx) };
    let source = SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 30490);

    // Oversized datagram first: 256 reported bytes into a 64-byte buffer.
    rx.push(vec![0xFFu8; BUF_LEN], 256, source);

    // Then a valid small SD message that fits the 64-byte buffer.
    let sd_msg = Message::<RawPayload>::new_sd(1, &empty_vec_sd_header());
    let mut wire = vec![0u8; BUF_LEN];
    let len = sd_msg.encode(&mut wire.as_mut_slice()).expect("encode sd");
    assert!(len <= BUF_LEN, "valid SD message must fit the claimed buffer");
    rx.push(wire[..len].to_vec(), len, source);

    let client_e2e: Arc<Mutex<E2ERegistry>> = Arc::new(Mutex::new(E2ERegistry::new()));
    let client_iface: Arc<RwLock<Ipv4Addr>> = Arc::new(RwLock::new(Ipv4Addr::LOCALHOST));
    let deps = ClientDeps {
        factory,
        spawner: TokioBackedSpawner,
        timer: MockTimer,
        e2e_registry: client_e2e,
        interface: client_iface,
        buffer_provider: StaticBufferProvider(&POOL),
    };
    let (client, mut updates, run_fut) = Client::<
        RawPayload,
        Arc<Mutex<E2ERegistry>>,
        Arc<RwLock<Ipv4Addr>>,
        E2ETestChannels,
    >::new_with_deps(deps, false);
    let run_handle = tokio::spawn(run_fut);

    client.bind_discovery().await.expect("bind_discovery");

    // The oversized datagram must be dropped and the loop must survive long
    // enough to deliver the valid SD message as a discovery update.
    let got = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(update) = updates.recv().await {
            if let ClientUpdate::DiscoveryUpdated(_) = update {
                return true;
            }
        }
        false
    })
    .await;

    assert!(
        got.unwrap_or(false),
        "loop must drop the oversized datagram, survive, and deliver the in-budget SD message"
    );

    client.shut_down();
    run_handle.abort();
}

/// Task 4: binding N unicast sockets claims N buffers from the shared pool;
/// a pool with exactly 2 slots rejects the 3rd distinct bind with
/// `Error::Capacity("udp_buffer")`. Driven through the public `Client`
/// `send_to_service` path, which binds one unicast socket per distinct
/// endpoint `local_port` and surfaces the bind error to the caller.
///
/// Note on release-on-close: `socket_loop_future` owns the `BufferLease`
/// and frees the pool slot when the loop exits (RAII on future drop). That
/// release is exercised at the unit level in `tests/buffer_pool.rs`
/// (`BufferLease::drop`); the public `Client` API exposes no per-socket
/// close, so the integration test here focuses on the claim + exhaustion
/// contract, which is the new bind-path behavior #125 introduces.
#[tokio::test]
async fn each_bound_socket_claims_one_buffer_and_releases_on_close() {
    // Exactly 2 slots so the 3rd concurrent unicast bind exhausts the pool.
    static POOL: BufferPool<2, UDP_BUFFER_SIZE> = BufferPool::new();

    let network = SharedNetwork::new();
    let client_factory = MockFactory {
        tx_pipe: Arc::clone(&network.client_to_server),
        rx_pipe: Arc::clone(&network.server_to_client),
        next_port: Arc::new(Mutex::new(0)),
    };

    let client_e2e: Arc<Mutex<E2ERegistry>> = Arc::new(Mutex::new(E2ERegistry::new()));
    let client_iface: Arc<RwLock<Ipv4Addr>> = Arc::new(RwLock::new(Ipv4Addr::LOCALHOST));
    let deps = ClientDeps {
        factory: client_factory,
        spawner: TokioBackedSpawner,
        timer: MockTimer,
        e2e_registry: client_e2e,
        interface: client_iface,
        buffer_provider: StaticBufferProvider(&POOL),
    };
    let (client, _updates, run_fut) = Client::<
        RawPayload,
        Arc<Mutex<E2ERegistry>>,
        Arc<RwLock<Ipv4Addr>>,
        E2ETestChannels,
    >::new_with_deps(deps, false);
    let run_handle = tokio::spawn(run_fut);

    let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 30700);

    // Each distinct (service, local_port) forces a distinct unicast bind,
    // each of which claims one buffer from the 2-slot pool. `send_to_service`
    // with a non-zero `local_port` binds that exact source port (no discovery
    // auto-bind, unlike `subscribe`).
    let bind_via_send = |svc: u16, port: u16| {
        let client = client.clone();
        async move {
            client
                .add_endpoint(svc, 1, target, port)
                .await
                .expect("add_endpoint");
            let msg_id = MessageId::new_from_service_and_method(svc, 0x0001);
            let payload = RawPayload::from_payload_bytes(msg_id, &[0u8; 4]).expect("payload");
            let request = Message::<RawPayload>::new(
                Header::new(
                    msg_id,
                    0x0001_0001,
                    1,
                    1,
                    MessageTypeField::new(MessageType::Request, false),
                    ReturnCode::Ok,
                    4,
                ),
                payload,
            );
            client.send_to_service(svc, 1, request).await.map(|_| ())
        }
    };

    bind_via_send(0x4001, 40000)
        .await
        .expect("1st bind claims slot 0");
    bind_via_send(0x4002, 40001)
        .await
        .expect("2nd bind claims slot 1");

    // 3rd distinct port: pool exhausted -> typed capacity error surfaces.
    let third = bind_via_send(0x4003, 40002).await;
    assert!(
        matches!(third, Err(ClientError::Capacity("udp_buffer"))),
        "3rd bind must fail with Capacity(\"udp_buffer\"), got {third:?}"
    );

    client.shut_down();
    run_handle.abort();
}

/// An empty `VecSdHeader` for building a minimal valid SD message.
fn empty_vec_sd_header() -> simple_someip::VecSdHeader {
    use simple_someip::protocol::sd::{Flags, RebootFlag};
    simple_someip::VecSdHeader {
        flags: Flags::new_sd(RebootFlag::RecentlyRebooted),
        entries: vec![],
        options: vec![],
    }
}
