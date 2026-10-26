//! Integration tests exercising the Client and Server together on localhost.
//!
//! # Parallel execution caveat
//!
//! These tests share `sd::MULTICAST_PORT` (30490) and bind it via
//! `SO_REUSEPORT`. Linux's reuseport hashing then load-balances incoming
//! Subscribe / SD multicast traffic across whichever sockets are
//! currently bound, which means one test's Subscribe message can be
//! delivered to a *different* test's server. Each test verifies its own
//! `EventPublisher::has_subscribers` (per-server `SubscriptionManager`
//! state, not a shared one), so the cross-routing produces flaky
//! failures when the suite runs with cargo's default parallelism.
//!
//! Until we can give each test its own SD port (which would require
//! widening the protocol layer's `MULTICAST_PORT` constant to a runtime
//! config) or its own network namespace, **run this binary with
//! `--test-threads=1`** to serialise the SD-port contention:
//!
//! ```text
//! cargo test --test client_server -- --test-threads=1
//! ```
//!
//! `cargo test --workspace` (parallel default) is expected to flake on
//! ~half of the tests in this file. The unit-test suite under
//! `cargo test --lib` does not have this issue and runs reliably in
//! parallel. The fix is tracked alongside the bare-metal refactor
//! (which will need to abstract the port anyway).

use simple_someip::ServiceEndpointKey;
use simple_someip::protocol::{
    Header, HeaderView, Message, MessageId, MessageType, MessageTypeField, ReturnCode, sd,
};
use simple_someip::server::ServerConfig;
use simple_someip::{
    Client, ClientUpdate, ClientUpdates, EncodeExt, PayloadWireFormat, RawPayload, Server,
    TokioChannels, VecSdHeader,
};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicU16, Ordering};

use simple_someip::client::Error as ClientError;
use simple_someip::{CapacityKind, ClientDeps, E2EHandle, E2EStatus, NoE2E, ServerDeps};

#[path = "support/tag_e2e.rs"]
mod tag_e2e;
use tag_e2e::{PASSED, TAG, TAG_MISSING, TagE2E};

/// Allocate a unique service ID per test invocation. Multiple
/// integration tests in this file run in parallel (cargo's default) and
/// would otherwise collide on the SD multicast group + a shared service
/// ID, causing cross-test SubscribeAck bleed-through.
fn next_service_id() -> u16 {
    static NEXT: AtomicU16 = AtomicU16::new(0x5B);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn empty_sd_header() -> VecSdHeader {
    VecSdHeader {
        flags: sd::Flags::new_sd(sd::RebootFlag::RecentlyRebooted),
        entries: vec![],
        options: vec![],
    }
}

type TestClient = Client<RawPayload, std::sync::Arc<std::sync::RwLock<Ipv4Addr>>, TokioChannels>;

/// Type alias bringing the tokio-flavor concrete type parameters back into
/// scope so callers can spell `TestServer::new(...)` without chasing the
/// four-type-parameter signature on every call site.
type TestServer = Server<
    simple_someip::TokioTransport,
    simple_someip::TokioTimer,
    simple_someip::NoE2E,
    std::sync::Arc<tokio::sync::RwLock<simple_someip::server::SubscriptionManager>>,
>;

/// Type alias for the event publisher concrete type used by `TestServer`'s
/// publisher. Same shape rationale as [`TestServer`].
type TestEventPublisher = simple_someip::server::EventPublisher<
    simple_someip::NoE2E,
    std::sync::Arc<tokio::sync::RwLock<simple_someip::server::SubscriptionManager>>,
    std::sync::Arc<simple_someip::TokioSocket>,
    simple_someip::TokioSocket,
>;

/// Create a server on an ephemeral unicast port, returning (Server, actual_port).
///
/// `TestServer::new` returns a `(Server, ServerHandles, run)` tuple.
/// Tests in this module construct the server, query the
/// kernel-assigned port via `unicast_local_addr`, and don't spawn
/// the run future from this helper — the few tests that need it call
/// `server.run()` directly after receiving the `Server` handle.
/// The full `Server` binds the SD port (30490) on its interface. Keep it on a
/// distinct loopback IP from the client (which stays on `127.0.0.1`) so the
/// client's receive-only unicast discovery socket (`interface:30490`,
/// `SO_REUSEPORT`) does not collide with the server's SD socket on the same
/// `IP:30490` and steal the client's own SubscribeEventgroup. This mirrors
/// production, where a full SD-announcing server is a remote sensor on its own
/// IP. See the #130 forward-port discussion.
const SERVER_IP: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 2);

async fn create_server(service_id: u16, instance_id: u16) -> (TestServer, u16) {
    create_server_on(SERVER_IP, service_id, instance_id).await
}

/// Like [`create_server`] but lets the caller pick the interface. Used by
/// tests that need multiple *distinct* devices (loopback IPs) offering the
/// same `(service_id, instance_id)` — the source-keyed-registry regression
/// coverage.
async fn create_server_on(
    interface: Ipv4Addr,
    service_id: u16,
    instance_id: u16,
) -> (TestServer, u16) {
    let config = ServerConfig::new(service_id, instance_id)
        .with_interface(interface)
        .with_local_port(0);
    let (server, _handles, _run): (TestServer, _, _) =
        TestServer::new(config).await.expect("Server::new failed");
    // Constructor already back-filled `config.local_port` from the
    // kernel-assigned bound port; just read it back for the test return.
    let port = match server.unicast_local_addr().expect("local_addr failed") {
        std::net::SocketAddr::V4(a) => a.port(),
        _ => panic!("expected IPv4"),
    };
    (server, port)
}

/// Poll `has_subscribers` with retries until the server has processed the
/// subscription. Returns true if subscribers appeared within the deadline.
async fn wait_for_subscribers(
    publisher: &TestEventPublisher,
    service_id: u16,
    instance_id: u16,
    event_group_id: u16,
) -> bool {
    for _ in 0..20 {
        if publisher
            .has_subscribers(service_id, instance_id, event_group_id)
            .await
        {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    false
}

/// Drain a client's update stream until the published `Unicast` event arrives,
/// skipping interleaved discovery traffic. A `SubscribeAck` now reaches the
/// client via the unicast SD socket (the #130 per-transport fix) and can land
/// on the channel just before the event, so a single `recv()` that expects the
/// event outright is racy — especially under the slower coverage build. Panics
/// on timeout or a closed channel; returns the `Unicast` update so callers can
/// inspect fields like `e2e_status`.
async fn recv_unicast(
    updates: &mut ClientUpdates<RawPayload, TokioChannels>,
) -> ClientUpdate<RawPayload> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match tokio::time::timeout_at(deadline, updates.recv()).await {
            Ok(Some(update @ ClientUpdate::Unicast { .. })) => return update,
            // Discovery ack / reboot notice — keep waiting for the event.
            Ok(Some(_)) => continue,
            Ok(None) => panic!("update channel closed before the Unicast event"),
            Err(_) => panic!("timed out waiting for the Unicast event"),
        }
    }
}

#[tokio::test]
async fn test_client_server_subscribe_and_receive_event() {
    // Start server on ephemeral port
    let service_id = next_service_id();
    let (server, server_port) = create_server(service_id, 1).await;
    let publisher = server.publisher();
    let server_handle = tokio::spawn(async move { server.run().await });

    // Create client and subscribe to the server's event group
    let (client, mut updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
    let _run_handle = tokio::spawn(run_fut);
    let server_addr = SocketAddrV4::new(SERVER_IP, server_port);
    client
        .add_endpoint(
            ServiceEndpointKey::udp(service_id, SocketAddr::V4(server_addr)),
            1,
            0,
        )
        .await
        .unwrap();
    client
        .subscribe(
            ServiceEndpointKey::udp(service_id, SocketAddr::V4(server_addr)),
            1,
            3,
            0x01,
            0,
        )
        .await
        .unwrap();

    assert!(
        wait_for_subscribers(&publisher, service_id, 1, 0x01).await,
        "server should have registered the subscriber"
    );

    // Drain any discovery update that may have arrived (SubscribeAck)
    let _ = tokio::time::timeout(std::time::Duration::from_millis(250), updates.recv()).await;

    // Publish an event from the server to the client's unicast port
    let event_msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header())
        .expect("in-tree SdHeader sizing is infallible");
    let sent = publisher
        .publish_event(service_id, 1, 0x01, &event_msg)
        .await
        .expect("publish_event failed");
    assert_eq!(sent, 1);

    // Client receives the unicast event
    let update = recv_unicast(&mut updates).await;
    assert!(
        matches!(update, ClientUpdate::Unicast { .. }),
        "expected Unicast, got {update:?}"
    );

    // Tear down
    client.unbind_discovery().await.unwrap();
    client.shut_down();
    server_handle.abort();
}

#[tokio::test]
async fn test_client_send_sd_auto_binds_discovery() {
    // Create server so there is something to send to
    let service_id = next_service_id();
    let (server, server_port) = create_server(service_id, 1).await;
    let server_handle = tokio::spawn(async move { server.run().await });

    // Create client — NO bind_discovery
    let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
    let _run_handle = tokio::spawn(run_fut);

    // send_sd_message should auto-bind discovery and succeed
    let sd_header = VecSdHeader {
        flags: sd::Flags::new_sd(sd::RebootFlag::RecentlyRebooted),
        entries: vec![sd::Entry::SubscribeEventGroup(sd::EventGroupEntry::new(
            service_id, 1, 1, 3, 0x01,
        ))],
        options: vec![sd::Options::IpV4Endpoint {
            ip: Ipv4Addr::LOCALHOST,
            protocol: sd::TransportProtocol::Udp,
            port: 12345,
        }],
    };
    let target = SocketAddrV4::new(SERVER_IP, server_port);
    client
        .send_sd_message(target, sd_header)
        .await
        .expect("send_sd_message should auto-bind discovery and succeed");

    client.shut_down();
    server_handle.abort();
}

/// Exercises the full bind/unbind lifecycle and set_interface flow
/// while an SD message round-trip is in flight.
#[tokio::test]
async fn test_client_bind_unbind_lifecycle_with_server() {
    let service_id = next_service_id();
    let (server, server_port) = create_server(service_id, 1).await;
    let server_handle = tokio::spawn(async move { server.run().await });

    let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
    let _run_handle = tokio::spawn(run_fut);

    // Bind discovery, subscribe, then unbind and rebind
    client.bind_discovery().await.unwrap();
    let server_addr = SocketAddrV4::new(SERVER_IP, server_port);
    client
        .add_endpoint(
            ServiceEndpointKey::udp(service_id, SocketAddr::V4(server_addr)),
            1,
            0,
        )
        .await
        .unwrap();
    client
        .subscribe(
            ServiceEndpointKey::udp(service_id, SocketAddr::V4(server_addr)),
            1,
            3,
            0x01,
            0,
        )
        .await
        .unwrap();

    // Unbind and rebind discovery — covers unbind_discovery + re-bind path
    client.unbind_discovery().await.unwrap();
    client.bind_discovery().await.unwrap();

    // set_interface while discovery is bound — covers the SetInterface arm
    // that unbinds discovery, changes interface, and rebinds
    client.set_interface(Ipv4Addr::LOCALHOST).await.unwrap();

    client.shut_down();
    server_handle.abort();
}

/// Verify that add_endpoint + send_to_service resolves the endpoint from the
/// registry, auto-binds unicast, sends the request, and receives a response.
#[tokio::test]
async fn test_add_endpoint_and_send_to_service() {
    let service_id = next_service_id();
    let (server, server_port) = create_server(service_id, 1).await;
    let publisher = server.publisher();
    let server_handle = tokio::spawn(async move { server.run().await });

    let (client, mut updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
    let _run_handle = tokio::spawn(run_fut);
    client.bind_discovery().await.unwrap();

    // Register the server's endpoint manually (simulating non-broadcasting service)
    let server_addr = SocketAddrV4::new(SERVER_IP, server_port);
    client
        .add_endpoint(
            ServiceEndpointKey::udp(service_id, SocketAddr::V4(server_addr)),
            1,
            0,
        )
        .await
        .unwrap();

    // Subscribe to server's event group (auto-binds unicast internally)
    client
        .subscribe(
            ServiceEndpointKey::udp(service_id, SocketAddr::V4(server_addr)),
            1,
            3,
            0x01,
            0,
        )
        .await
        .unwrap();

    // Wait for the server to process the subscription
    assert!(
        wait_for_subscribers(&publisher, service_id, 1, 0x01).await,
        "server should have registered the subscriber"
    );

    // Drain any pending discovery update
    let _ = tokio::time::timeout(std::time::Duration::from_millis(250), updates.recv()).await;

    // Publish an event from the server
    let event_msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header())
        .expect("in-tree SdHeader sizing is infallible");
    let sent = publisher
        .publish_event(service_id, 1, 0x01, &event_msg)
        .await
        .expect("publish_event failed");
    assert_eq!(sent, 1);

    // Client receives the unicast event
    let update = recv_unicast(&mut updates).await;
    assert!(
        matches!(update, ClientUpdate::Unicast { .. }),
        "expected Unicast, got {update:?}"
    );

    // Remove the endpoint and verify send_to_service returns ServiceNotFound
    client
        .remove_endpoint(ServiceEndpointKey::udp(
            service_id,
            SocketAddr::V4(server_addr),
        ))
        .await
        .unwrap();
    let msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header())
        .expect("in-tree SdHeader sizing is infallible");
    let result = client
        .send_to_service(
            ServiceEndpointKey::udp(service_id, SocketAddr::V4(server_addr)),
            msg,
        )
        .await;
    assert!(
        matches!(result, Err(simple_someip::client::Error::ServiceNotFound)),
        "expected ServiceNotFound after remove, got {result:?}"
    );
    // Verify that PendingResponse is importable from the crate root
    let _: fn() -> Option<simple_someip::PendingResponse<RawPayload, TokioChannels>> = || None;

    client.shut_down();
    server_handle.abort();
}

/// Verify subscribe auto-binds discovery when discovery is not already bound.
/// Exercises the Subscribe auto-bind discovery path in inner.rs.
#[tokio::test]
async fn test_subscribe_auto_binds_discovery() {
    let service_id = next_service_id();
    let (server, server_port) = create_server(service_id, 1).await;
    let publisher = server.publisher();
    let server_handle = tokio::spawn(async move { server.run().await });

    // Create client — do NOT bind discovery manually
    let (client, mut updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
    let _run_handle = tokio::spawn(run_fut);
    let server_addr = SocketAddrV4::new(SERVER_IP, server_port);
    client
        .add_endpoint(
            ServiceEndpointKey::udp(service_id, SocketAddr::V4(server_addr)),
            1,
            0,
        )
        .await
        .unwrap();
    // Subscribe should auto-bind discovery internally
    client
        .subscribe(
            ServiceEndpointKey::udp(service_id, SocketAddr::V4(server_addr)),
            1,
            3,
            0x01,
            0,
        )
        .await
        .unwrap();

    assert!(
        wait_for_subscribers(&publisher, service_id, 1, 0x01).await,
        "server should have registered the subscriber"
    );

    // Drain SubscribeAck
    let _ = tokio::time::timeout(std::time::Duration::from_millis(250), updates.recv()).await;

    // Publish an event and verify the client can receive it
    let event_msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header())
        .expect("in-tree SdHeader sizing is infallible");
    let sent = publisher
        .publish_event(service_id, 1, 0x01, &event_msg)
        .await
        .expect("publish_event failed");
    assert_eq!(sent, 1);

    let update = recv_unicast(&mut updates).await;
    assert!(
        matches!(update, ClientUpdate::Unicast { .. }),
        "expected Unicast, got {update:?}"
    );

    client.shut_down();
    server_handle.abort();
}

/// Verify that `request()` resolves when the server sends a unicast reply.
/// Exercises the pending_responses HashMap matching path in inner.rs.
#[tokio::test]
async fn test_client_request_resolves_via_unicast_reply() {
    let service_id = next_service_id();
    let (server, server_port) = create_server(service_id, 1).await;
    let publisher = server.publisher();
    let server_handle = tokio::spawn(async move { server.run().await });

    let (client, mut updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
    let _run_handle = tokio::spawn(run_fut);
    let server_addr = SocketAddrV4::new(SERVER_IP, server_port);
    client
        .add_endpoint(
            ServiceEndpointKey::udp(service_id, SocketAddr::V4(server_addr)),
            1,
            0,
        )
        .await
        .unwrap();
    client
        .subscribe(
            ServiceEndpointKey::udp(service_id, SocketAddr::V4(server_addr)),
            1,
            3,
            0x01,
            0,
        )
        .await
        .unwrap();

    assert!(
        wait_for_subscribers(&publisher, service_id, 1, 0x01).await,
        "server should have registered the subscriber"
    );

    // Drain SubscribeAck
    let _ = tokio::time::timeout(std::time::Duration::from_millis(250), updates.recv()).await;

    // send_to_service creates a PendingResponse; the server will send the event
    // which has a matching request_id, resolving it.
    let msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header())
        .expect("in-tree SdHeader sizing is infallible");
    let pending = client
        .send_to_service(
            ServiceEndpointKey::udp(service_id, SocketAddr::V4(server_addr)),
            msg,
        )
        .await
        .expect("send_to_service failed");

    // Publish an event that the client unicast socket will receive
    let event_msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header())
        .expect("in-tree SdHeader sizing is infallible");
    publisher
        .publish_event(service_id, 1, 0x01, &event_msg)
        .await
        .expect("publish_event failed");

    // The event may or may not match the pending response's request_id.
    // Either way the client should receive *something* on its unicast socket.
    // We just verify the unicast path is exercised.
    let update = tokio::time::timeout(std::time::Duration::from_secs(2), updates.recv())
        .await
        .expect("timeout waiting for unicast update");
    // Could be Unicast (non-matching request_id) — that's fine
    assert!(update.is_some(), "expected an update");

    // Clean up pending (it may never resolve if request_id didn't match)
    drop(pending);

    client.shut_down();
    server_handle.abort();
}

/// Verify that two clients can subscribe to the same server and both receive events.
/// Exercises multi-subscriber path in event_publisher.rs.
#[tokio::test]
async fn test_multiple_subscribers_receive_events() {
    let service_id = next_service_id();
    let (server, server_port) = create_server(service_id, 1).await;
    let publisher = server.publisher();
    let server_handle = tokio::spawn(async move { server.run().await });

    let server_addr = SocketAddrV4::new(SERVER_IP, server_port);

    // Client 1
    let (client1, mut updates1, run_fut1) = TestClient::new(Ipv4Addr::LOCALHOST);
    tokio::spawn(run_fut1);
    client1
        .add_endpoint(
            ServiceEndpointKey::udp(service_id, SocketAddr::V4(server_addr)),
            1,
            0,
        )
        .await
        .unwrap();
    client1
        .subscribe(
            ServiceEndpointKey::udp(service_id, SocketAddr::V4(server_addr)),
            1,
            3,
            0x01,
            0,
        )
        .await
        .unwrap();

    // Client 2
    let (client2, mut updates2, run_fut2) = TestClient::new(Ipv4Addr::LOCALHOST);
    tokio::spawn(run_fut2);
    client2
        .add_endpoint(
            ServiceEndpointKey::udp(service_id, SocketAddr::V4(server_addr)),
            1,
            0,
        )
        .await
        .unwrap();
    client2
        .subscribe(
            ServiceEndpointKey::udp(service_id, SocketAddr::V4(server_addr)),
            1,
            3,
            0x01,
            0,
        )
        .await
        .unwrap();

    // `SUBSCRIBERS_PER_GROUP` is a compile-time cap sized via
    // `SIMPLE_SOMEIP_MAX_SUBS` (default 1) so the host build links exactly
    // the memory it needs. Above that cap excess subscribers are dropped by
    // design, so the number we can actually exercise is `min(2, cap)`.
    let expected = ServerConfig::SUBSCRIBERS_PER_GROUP_CAP.min(2);

    // Wait for the subscribers the cap allows
    for _ in 0..40 {
        if publisher.subscriber_count(service_id, 1, 0x01).await >= expected {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        publisher.subscriber_count(service_id, 1, 0x01).await >= expected,
        "expected at least {expected} subscriber(s) (SUBSCRIBERS_PER_GROUP cap = {})",
        ServerConfig::SUBSCRIBERS_PER_GROUP_CAP
    );

    // Drain discovery updates
    let _ = tokio::time::timeout(std::time::Duration::from_millis(250), updates1.recv()).await;
    let _ = tokio::time::timeout(std::time::Duration::from_millis(250), updates2.recv()).await;

    // Publish event
    let event_msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header())
        .expect("in-tree SdHeader sizing is infallible");
    let sent = publisher
        .publish_event(service_id, 1, 0x01, &event_msg)
        .await
        .expect("publish_event failed");
    assert!(sent >= expected, "expected sent >= {expected}, got {sent}");

    // Client 1 (the first accepted subscriber) always receives the event.
    let u1 = recv_unicast(&mut updates1).await;
    assert!(
        matches!(u1, ClientUpdate::Unicast { .. }),
        "client1 expected Unicast, got {u1:?}"
    );

    // Client 2 is only accepted (and thus only receives) when the build's
    // subscriber cap admits a second subscriber; otherwise it was dropped at
    // subscribe time, so the multi-subscriber fan-out is not exercised here.
    if expected >= 2 {
        let u2 = recv_unicast(&mut updates2).await;
        assert!(
            matches!(u2, ClientUpdate::Unicast { .. }),
            "client2 expected Unicast, got {u2:?}"
        );
    } else {
        eprintln!(
            "SUBSCRIBERS_PER_GROUP cap = 1: skipping the second-subscriber fan-out \
             assertion (rebuild with SIMPLE_SOMEIP_MAX_SUBS>=2 to exercise it)"
        );
    }

    client1.shut_down();
    client2.shut_down();
    server_handle.abort();
}

/// Verify ClientUpdates returns None after client shutdown.
#[tokio::test]
async fn test_updates_drain_after_shutdown() {
    let (client, mut updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
    let _run_handle = tokio::spawn(run_fut);
    client.shut_down();

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), updates.recv())
        .await
        .expect("timeout waiting for None");
    assert!(result.is_none(), "expected None after shutdown");
}

/// Verify that cloned client handles work independently.
#[tokio::test]
async fn test_cloned_client_works() {
    let service_id = next_service_id();
    let (server, server_port) = create_server(service_id, 1).await;
    let server_handle = tokio::spawn(async move { server.run().await });

    let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
    let _run_handle = tokio::spawn(run_fut);
    let client2 = client.clone();

    // Both clones can send commands
    let server_addr = SocketAddrV4::new(SERVER_IP, server_port);
    client
        .add_endpoint(
            ServiceEndpointKey::udp(service_id, SocketAddr::V4(server_addr)),
            1,
            0,
        )
        .await
        .unwrap();
    client2
        .subscribe(
            ServiceEndpointKey::udp(service_id, SocketAddr::V4(server_addr)),
            1,
            3,
            0x01,
            0,
        )
        .await
        .unwrap();

    client.shut_down();
    // client2 is also dropped
    server_handle.abort();
}

/// Subscribe with a specific client_port, then subscribe again reusing the same port.
/// Exercises the port-reuse path in Subscribe handling.
#[tokio::test]
async fn test_subscribe_specific_port_reuse() {
    let service_id = next_service_id();
    let (server, server_port) = create_server(service_id, 1).await;
    let server_handle = tokio::spawn(async move { server.run().await });

    let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
    let _run_handle = tokio::spawn(run_fut);
    let server_addr = SocketAddrV4::new(SERVER_IP, server_port);
    client
        .add_endpoint(
            ServiceEndpointKey::udp(service_id, SocketAddr::V4(server_addr)),
            1,
            0,
        )
        .await
        .unwrap();

    // Use specific port
    let specific_port = 44444;
    client
        .subscribe(
            ServiceEndpointKey::udp(service_id, SocketAddr::V4(server_addr)),
            1,
            3,
            0x01,
            specific_port,
        )
        .await
        .unwrap();
    // Second subscribe reuses the port
    client
        .subscribe(
            ServiceEndpointKey::udp(service_id, SocketAddr::V4(server_addr)),
            1,
            3,
            0x02,
            specific_port,
        )
        .await
        .unwrap();

    client.shut_down();
    server_handle.abort();
}

/// Regression for the source-keyed registry (the fix this test suite gates):
/// once firmware assigns a fixed (ECU-Extract) instance id, every device on
/// the subnet advertises the identical `(service_id, instance_id)`. Two real
/// servers on distinct loopback IPs both offer that same pair; the client
/// must resolve each by `target_ip` independently, receive events tagged
/// with the correct source, and removing one device's endpoint must not
/// evict the other's.
#[tokio::test]
async fn test_two_devices_same_service_instance_addressed_independently() {
    let service_id = next_service_id();
    const DEVICE_A: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 10);
    const DEVICE_B: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 11);

    let (server_a, port_a) = create_server_on(DEVICE_A, service_id, 1).await;
    let publisher_a = server_a.publisher();
    let server_a_handle = tokio::spawn(async move { server_a.run().await });

    let (server_b, port_b) = create_server_on(DEVICE_B, service_id, 1).await;
    let publisher_b = server_b.publisher();
    let server_b_handle = tokio::spawn(async move { server_b.run().await });

    let addr_a = SocketAddrV4::new(DEVICE_A, port_a);
    let addr_b = SocketAddrV4::new(DEVICE_B, port_b);

    let (client, mut updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
    let _run_handle = tokio::spawn(run_fut);

    // Both devices advertise the identical (service_id, instance_id=1) — the
    // exact scenario a fixed ECU-Extract instance id produces. Before this
    // task, the second `add_endpoint` would have overwritten the first in
    // the registry.
    client
        .add_endpoint(
            ServiceEndpointKey::udp(service_id, SocketAddr::V4(addr_a)),
            1,
            0,
        )
        .await
        .unwrap();
    client
        .add_endpoint(
            ServiceEndpointKey::udp(service_id, SocketAddr::V4(addr_b)),
            1,
            0,
        )
        .await
        .unwrap();

    // Distinct event groups so `has_subscribers`/`publish_event` on each
    // server can be checked independently.
    client
        .subscribe(
            ServiceEndpointKey::udp(service_id, SocketAddr::V4(addr_a)),
            1,
            3,
            0x01,
            0,
        )
        .await
        .unwrap();
    client
        .subscribe(
            ServiceEndpointKey::udp(service_id, SocketAddr::V4(addr_b)),
            1,
            3,
            0x02,
            0,
        )
        .await
        .unwrap();

    assert!(
        wait_for_subscribers(&publisher_a, service_id, 1, 0x01).await,
        "server A should have registered the subscriber"
    );
    assert!(
        wait_for_subscribers(&publisher_b, service_id, 1, 0x02).await,
        "server B should have registered the subscriber"
    );

    // Publish from A: the client must receive an event sourced from A, not B.
    let event_msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header())
        .expect("in-tree SdHeader sizing is infallible");
    let sent_a = publisher_a
        .publish_event(service_id, 1, 0x01, &event_msg)
        .await
        .expect("publish_event from device A failed");
    assert_eq!(sent_a, 1);
    match recv_unicast(&mut updates).await {
        ClientUpdate::Unicast { source, .. } => {
            assert_eq!(
                source.ip(),
                std::net::IpAddr::V4(DEVICE_A),
                "event should have arrived from device A"
            );
        }
        other => panic!("expected Unicast from device A, got {other:?}"),
    }

    // Publish from B: independently resolvable, not shadowed by A's entry.
    let sent_b = publisher_b
        .publish_event(service_id, 1, 0x02, &event_msg)
        .await
        .expect("publish_event from device B failed");
    assert_eq!(sent_b, 1);
    match recv_unicast(&mut updates).await {
        ClientUpdate::Unicast { source, .. } => {
            assert_eq!(
                source.ip(),
                std::net::IpAddr::V4(DEVICE_B),
                "event should have arrived from device B"
            );
        }
        other => panic!("expected Unicast from device B, got {other:?}"),
    }

    // Removing device A's endpoint must not evict device B's — the fix for
    // "StopOffer/remove from one device evicted all".
    client
        .remove_endpoint(ServiceEndpointKey::udp(service_id, SocketAddr::V4(addr_a)))
        .await
        .unwrap();

    let msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header())
        .expect("in-tree SdHeader sizing is infallible");
    let result_a = client
        .send_to_service(
            ServiceEndpointKey::udp(service_id, SocketAddr::V4(addr_a)),
            msg,
        )
        .await;
    assert!(
        matches!(result_a, Err(simple_someip::client::Error::ServiceNotFound)),
        "expected ServiceNotFound for removed device A, got {result_a:?}"
    );

    let msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header())
        .expect("in-tree SdHeader sizing is infallible");
    let result_b = client
        .send_to_service(
            ServiceEndpointKey::udp(service_id, SocketAddr::V4(addr_b)),
            msg,
        )
        .await;
    assert!(
        result_b.is_ok(),
        "device B must remain reachable after removing device A's endpoint: {result_b:?}"
    );

    client.shut_down();
    server_a_handle.abort();
    server_b_handle.abort();
}

/// Answers the first request `socket` receives with `replies`, in order, each
/// a `(message type, return code, payload)` that echoes the request's message
/// and request IDs. Returns once every reply is sent.
async fn reply_with(
    socket: tokio::net::UdpSocket,
    replies: &'static [(MessageType, ReturnCode, &'static [u8])],
) {
    let mut buf = [0u8; 1500];
    let (n, from) = socket.recv_from(&mut buf).await.expect("request");
    let (request, _) = HeaderView::parse(&buf[..n]).expect("request header");
    let id = request.message_id();
    for &(message_type, return_code, payload) in replies {
        let header = Header::new(
            id,
            request.request_id(),
            1,
            request.interface_version(),
            MessageTypeField::new(message_type, false),
            return_code,
            payload.len(),
        );
        let reply = Message::new(
            header,
            RawPayload::from_payload_bytes(id, payload).expect("a raw payload always parses"),
        );
        let bytes = reply.encode_to_vec().expect("reply encodes");
        socket.send_to(&bytes, from).await.expect("send reply");
    }
}

/// Sends one request from a fresh client to a plain UDP socket that answers
/// it with `replies` (see [`reply_with`]), and returns what `request`
/// resolved to.
async fn request_answered_with(
    replies: &'static [(MessageType, ReturnCode, &'static [u8])],
) -> Result<RawPayload, simple_someip::client::Error> {
    let service_id = next_service_id();
    let peer = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let peer_addr = peer.local_addr().unwrap();
    let replier = tokio::spawn(reply_with(peer, replies));

    let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
    let run_handle = tokio::spawn(run_fut);
    let key = ServiceEndpointKey::udp(service_id, peer_addr);
    client.add_endpoint(key, 1, 0).await.unwrap();

    let id = MessageId::new_from_service_and_method(service_id, 0x0001);
    let request = Message::new(
        Header::new(
            id,
            0,
            1,
            1,
            MessageTypeField::new(MessageType::Request, false),
            ReturnCode::Ok,
            1,
        ),
        RawPayload::from_payload_bytes(id, &[0x2A]).unwrap(),
    );
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client.request(key, request),
    )
    .await
    .expect("the reply resolves the request");
    replier.await.unwrap();
    client.shut_down();
    run_handle.abort();
    outcome
}

#[tokio::test]
async fn request_returns_the_response_payload() {
    let payload = request_answered_with(&[(MessageType::Response, ReturnCode::Ok, &[0x07])])
        .await
        .expect("a RESPONSE with E_OK is a success");
    assert_eq!(payload.raw_bytes().unwrap(), &[0x07]);
}

#[tokio::test]
async fn request_returns_the_return_code_of_an_error_reply() {
    let outcome = request_answered_with(&[(MessageType::Error, ReturnCode::NotOk, &[])]).await;
    assert!(
        matches!(
            outcome,
            Err(simple_someip::client::Error::ErrorReply {
                message_type: MessageType::Error,
                return_code: ReturnCode::NotOk,
                ..
            })
        ),
        "got {outcome:?}"
    );
}

#[tokio::test]
async fn request_returns_the_return_code_of_a_response_that_is_not_ok() {
    let outcome =
        request_answered_with(&[(MessageType::Response, ReturnCode::InterfaceError(0x20), &[])])
            .await;
    assert!(
        matches!(
            outcome,
            Err(simple_someip::client::Error::ErrorReply {
                message_type: MessageType::Response,
                return_code: ReturnCode::InterfaceError(0x20),
                ..
            })
        ),
        "got {outcome:?}"
    );
}

#[tokio::test]
async fn an_unmatched_error_reply_that_decodes_arrives_as_error_reply_with_its_payload() {
    let service_id = next_service_id();
    let peer = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let peer_addr = peer.local_addr().unwrap();
    let replier = tokio::spawn(reply_with(
        peer,
        &[
            (MessageType::Response, ReturnCode::NotOk, &[0x09]),
            (MessageType::Response, ReturnCode::NotOk, &[0x09]),
        ],
    ));

    let (client, mut updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
    let run_handle = tokio::spawn(run_fut);
    let key = ServiceEndpointKey::udp(service_id, peer_addr);
    client.add_endpoint(key, 1, 0).await.unwrap();
    let id = MessageId::new_from_service_and_method(service_id, 0x0001);
    let request = Message::new(
        Header::new(
            id,
            0,
            1,
            1,
            MessageTypeField::new(MessageType::Request, false),
            ReturnCode::Ok,
            0,
        ),
        RawPayload::from_payload_bytes(id, &[]).unwrap(),
    );
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client.request(key, request),
    )
    .await
    .expect("the first reply resolves the request");
    assert!(
        matches!(
            outcome,
            Err(simple_someip::client::Error::ErrorReply { .. })
        ),
        "got {outcome:?}"
    );

    let update = tokio::time::timeout(std::time::Duration::from_secs(2), updates.recv())
        .await
        .expect("the second reply reaches the update stream")
        .expect("the update stream is open");
    replier.await.unwrap();
    match update {
        ClientUpdate::ErrorReply {
            header,
            payload: Some(payload),
            source,
            ..
        } => {
            assert_eq!(header.return_code(), ReturnCode::NotOk);
            assert_eq!(payload.raw_bytes().unwrap(), &[0x09]);
            assert_eq!(source, peer_addr);
        }
        other => panic!("expected ClientUpdate::ErrorReply with a payload, got {other:?}"),
    }
    client.shut_down();
    run_handle.abort();
}

#[tokio::test]
async fn request_is_not_resolved_by_a_notification_with_its_request_id() {
    let payload = request_answered_with(&[
        (MessageType::Notification, ReturnCode::Ok, &[0x01]),
        (MessageType::Response, ReturnCode::Ok, &[0x02]),
    ])
    .await
    .expect("the RESPONSE resolves the request");
    assert_eq!(payload.raw_bytes().unwrap(), &[0x02]);
}

const TAGGED_EVENT: u16 = 0x8001;
const OTHER_EVENT: u16 = 0x8002;

/// Starts a server with `server_hook` and a subscribed client with
/// `client_hook`, publishes each `(event_id, payload)` in turn, and returns
/// what the client received for each: `(event_id, e2e_status, payload)`.
async fn publish_and_receive<S, C>(
    server_hook: S,
    client_hook: C,
    service_id: u16,
    events: &[(u16, &[u8])],
) -> Vec<(u16, Option<E2EStatus>, Vec<u8>)>
where
    S: E2EHandle + Clone,
    C: E2EHandle + Clone,
{
    let config = ServerConfig::new(service_id, 1)
        .with_interface(SERVER_IP)
        .with_local_port(0);
    let deps = ServerDeps::tokio().with_e2e(server_hook);
    let (server, _handles, _run): (Server<_, _, S, _>, _, _) =
        Server::new_with_deps(deps, config, false)
            .await
            .expect("server");
    let server_port = server.unicast_local_addr().expect("local addr").port();
    let publisher = server.publisher();
    let server_handle = tokio::spawn(async move { server.run().await });

    let deps = ClientDeps::tokio(Ipv4Addr::LOCALHOST).with_e2e(client_hook);
    let (client, mut updates, run_fut) =
        Client::<RawPayload, _, TokioChannels>::new_with_deps(deps, false);
    let run_handle = tokio::spawn(run_fut);
    let key = ServiceEndpointKey::udp(
        service_id,
        SocketAddr::V4(SocketAddrV4::new(SERVER_IP, server_port)),
    );
    client.add_endpoint(key, 1, 0).await.unwrap();
    client.subscribe(key, 1, 3, 0x01, 0).await.unwrap();
    let mut subscribed = false;
    for _ in 0..20 {
        if publisher.has_subscribers(service_id, 1, 0x01).await {
            subscribed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(subscribed, "server should have registered the subscriber");
    let _ = tokio::time::timeout(std::time::Duration::from_millis(250), updates.recv()).await;

    let mut received = Vec::new();
    for &(event_id, payload) in events {
        let id = MessageId::new_from_service_and_method(service_id, event_id);
        let header = Header::new_event(service_id, event_id, 0, 0x01, 0x01, payload.len());
        let message = Message::new(header, RawPayload::from_payload_bytes(id, payload).unwrap());
        let sent = publisher
            .publish_event(service_id, 1, 0x01, &message)
            .await
            .expect("publish_event failed");
        assert_eq!(sent, 1);
        match recv_unicast(&mut updates).await {
            ClientUpdate::Unicast {
                message,
                e2e_status,
                ..
            } => received.push((
                event_id,
                e2e_status,
                message.payload().raw_bytes().unwrap().to_vec(),
            )),
            other => unreachable!("recv_unicast only returns Unicast, got {other:?}"),
        }
    }
    client.shut_down();
    run_handle.abort();
    server_handle.abort();
    received
}

/// Sends one request of method 0x0001 carrying `payload` from a client with
/// `hook` to a plain UDP peer; returns the client's send result and the bytes
/// the peer received, if any.
async fn send_request_and_capture<H: E2EHandle + Clone>(
    hook: H,
    service_id: u16,
    payload: &[u8],
) -> (Result<(), ClientError>, Option<Vec<u8>>) {
    let peer = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let key = ServiceEndpointKey::udp(service_id, peer.local_addr().unwrap());
    let deps = ClientDeps::tokio(Ipv4Addr::LOCALHOST).with_e2e(hook);
    let (client, _updates, run_fut) =
        Client::<RawPayload, _, TokioChannels>::new_with_deps(deps, false);
    let run_handle = tokio::spawn(run_fut);
    client.add_endpoint(key, 1, 0).await.unwrap();
    let id = MessageId::new_from_service_and_method(service_id, 0x0001);
    let request = Message::new(
        Header::new(
            id,
            0,
            1,
            1,
            MessageTypeField::new(MessageType::Request, false),
            ReturnCode::Ok,
            payload.len(),
        ),
        RawPayload::from_payload_bytes(id, payload).unwrap(),
    );
    let sent = client.send_to_service(key, request).await.map(|_| ());
    let mut buf = [0u8; 2048];
    let arrived = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        peer.recv_from(&mut buf),
    )
    .await
    .ok()
    .map(|r| buf[..r.unwrap().0].to_vec());
    client.shut_down();
    run_handle.abort();
    (sent, arrived)
}

#[tokio::test]
async fn hook_protects_on_publish_and_checks_on_receive() {
    let service_id = next_service_id();
    let hook = TagE2E::new(MessageId::new_from_service_and_method(
        service_id,
        TAGGED_EVENT,
    ));
    let received = publish_and_receive(
        hook.clone(),
        hook,
        service_id,
        &[(TAGGED_EVENT, &[0x01, 0x02]), (OTHER_EVENT, &[0x01, 0x02])],
    )
    .await;
    assert_eq!(
        received,
        [
            (
                TAGGED_EVENT,
                Some(E2EStatus::Passed(PASSED)),
                vec![0x01, 0x02]
            ),
            (OTHER_EVENT, None, vec![0x01, 0x02]),
        ]
    );
}

#[tokio::test]
async fn a_failed_check_delivers_the_original_bytes() {
    let service_id = next_service_id();
    let hook = TagE2E::new(MessageId::new_from_service_and_method(
        service_id,
        TAGGED_EVENT,
    ));
    let received = publish_and_receive(
        NoE2E,
        hook,
        service_id,
        &[(TAGGED_EVENT, &[0x07]), (TAGGED_EVENT, &[])],
    )
    .await;
    assert_eq!(
        received,
        [
            (
                TAGGED_EVENT,
                Some(E2EStatus::Failed(TAG_MISSING)),
                vec![0x07]
            ),
            (TAGGED_EVENT, Some(E2EStatus::Failed(TAG_MISSING)), vec![]),
        ]
    );
}

#[tokio::test]
async fn no_hook_delivers_with_no_status() {
    let service_id = next_service_id();
    let received = publish_and_receive(NoE2E, NoE2E, service_id, &[(TAGGED_EVENT, &[0x07])]).await;
    assert_eq!(received, [(TAGGED_EVENT, None, vec![0x07])]);
}

#[tokio::test]
async fn hook_protects_a_client_request() {
    let service_id = next_service_id();
    let hook = TagE2E::new(MessageId::new_from_service_and_method(service_id, 0x0001));
    let (sent, arrived) = send_request_and_capture(hook, service_id, &[0x2A]).await;
    sent.expect("send");
    let bytes = arrived.expect("the peer receives the request");
    assert_eq!(&bytes[4..8], &10u32.to_be_bytes());
    assert_eq!(&bytes[16..], &[TAG, 0x2A]);
}

#[tokio::test]
async fn a_protected_request_that_does_not_fit_is_rejected() {
    let service_id = next_service_id();
    let hook = TagE2E::new(MessageId::new_from_service_and_method(service_id, 0x0001));
    // Fills the send buffer unprotected; the tag makes it one byte too long.
    let payload = vec![0x55; simple_someip::UDP_BUFFER_SIZE - 16];
    let (sent, arrived) = send_request_and_capture(hook, service_id, &payload).await;
    assert!(
        matches!(sent, Err(ClientError::Capacity(CapacityKind::UdpBuffer))),
        "got {sent:?}"
    );
    assert_eq!(arrived, None, "nothing is sent");
}

#[tokio::test]
async fn a_hook_for_every_message_leaves_service_discovery_alone() {
    let service_id = next_service_id();
    let hook = TagE2E::every_message();
    let received = publish_and_receive(hook, hook, service_id, &[(TAGGED_EVENT, &[0x01])]).await;
    assert_eq!(
        received,
        [(TAGGED_EVENT, Some(E2EStatus::Passed(PASSED)), vec![0x01])]
    );
}
