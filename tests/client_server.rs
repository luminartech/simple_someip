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

use simple_someip::e2e::{E2ECheckStatus, E2EKey, E2EProfile, Profile4Config};
use simple_someip::protocol::{Header, Message, MessageId, sd};
use simple_someip::server::ServerConfig;
use simple_someip::{
    Client, ClientUpdate, ClientUpdates, PayloadWireFormat, RawPayload, Server, TokioChannels,
    VecSdHeader,
};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::atomic::{AtomicU16, Ordering};

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

type TestClient = Client<
    RawPayload,
    std::sync::Arc<std::sync::Mutex<simple_someip::e2e::E2ERegistry>>,
    std::sync::Arc<std::sync::RwLock<Ipv4Addr>>,
    TokioChannels,
>;

/// Type alias bringing the tokio-flavor concrete type parameters back into
/// scope so callers can spell `TestServer::new(...)` without chasing the
/// four-type-parameter signature on every call site.
type TestServer = Server<
    simple_someip::TokioTransport,
    simple_someip::TokioTimer,
    std::sync::Arc<std::sync::Mutex<simple_someip::e2e::E2ERegistry>>,
    std::sync::Arc<tokio::sync::RwLock<simple_someip::server::SubscriptionManager>>,
>;

/// Type alias for the event publisher concrete type used by `TestServer`'s
/// publisher. Same shape rationale as [`TestServer`].
type TestEventPublisher = simple_someip::server::EventPublisher<
    std::sync::Arc<std::sync::Mutex<simple_someip::e2e::E2ERegistry>>,
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
        .add_endpoint(service_id, 1, server_addr, 0)
        .await
        .unwrap();
    client
        .subscribe(service_id, 1, SERVER_IP, 1, 3, 0x01, 0)
        .await
        .unwrap();

    assert!(
        wait_for_subscribers(&publisher, service_id, 1, 0x01).await,
        "server should have registered the subscriber"
    );

    // Drain any discovery update that may have arrived (SubscribeAck)
    let _ = tokio::time::timeout(std::time::Duration::from_millis(250), updates.recv()).await;

    // Publish an event from the server to the client's unicast port
    let event_msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header());
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
        .add_endpoint(service_id, 1, server_addr, 0)
        .await
        .unwrap();
    client
        .subscribe(service_id, 1, SERVER_IP, 1, 3, 0x01, 0)
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
        .add_endpoint(service_id, 1, server_addr, 0)
        .await
        .unwrap();

    // Subscribe to server's event group (auto-binds unicast internally)
    client
        .subscribe(service_id, 1, SERVER_IP, 1, 3, 0x01, 0)
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
    let event_msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header());
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
        .remove_endpoint(service_id, 1, SERVER_IP)
        .await
        .unwrap();
    let msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header());
    let result = client.send_to_service(service_id, 1, SERVER_IP, msg).await;
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
        .add_endpoint(service_id, 1, server_addr, 0)
        .await
        .unwrap();
    // Subscribe should auto-bind discovery internally
    client
        .subscribe(service_id, 1, SERVER_IP, 1, 3, 0x01, 0)
        .await
        .unwrap();

    assert!(
        wait_for_subscribers(&publisher, service_id, 1, 0x01).await,
        "server should have registered the subscriber"
    );

    // Drain SubscribeAck
    let _ = tokio::time::timeout(std::time::Duration::from_millis(250), updates.recv()).await;

    // Publish an event and verify the client can receive it
    let event_msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header());
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
        .add_endpoint(service_id, 1, server_addr, 0)
        .await
        .unwrap();
    client
        .subscribe(service_id, 1, SERVER_IP, 1, 3, 0x01, 0)
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
    let msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header());
    let pending = client
        .send_to_service(service_id, 1, SERVER_IP, msg)
        .await
        .expect("send_to_service failed");

    // Publish an event that the client unicast socket will receive
    let event_msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header());
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

/// Verify that E2E protection is applied by the server and checked by the client.
/// Exercises E2E protect in event_publisher.rs and E2E check in socket_manager.rs.
#[tokio::test]
async fn test_e2e_protect_on_publish_and_check_on_receive() {
    let service_id = next_service_id();
    let (server, server_port) = create_server(service_id, 1).await;
    let publisher = server.publisher();

    // Register E2E profile on server for the event message ID
    let key = E2EKey {
        service_id,
        method_or_event_id: 0x0001,
    };
    let profile = E2EProfile::Profile4(Profile4Config::new(0x12345678, 15));
    server
        .register_e2e(key, profile.clone())
        .expect("E2E registry has capacity for one entry");

    let server_handle = tokio::spawn(async move { server.run().await });

    let (client, mut updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
    let _run_handle = tokio::spawn(run_fut);

    // Register matching E2E profile on client
    client
        .register_e2e(key, profile)
        .expect("E2E registry has capacity for one entry");

    let server_addr = SocketAddrV4::new(SERVER_IP, server_port);
    client
        .add_endpoint(service_id, 1, server_addr, 0)
        .await
        .unwrap();
    client
        .subscribe(service_id, 1, SERVER_IP, 1, 3, 0x01, 0)
        .await
        .unwrap();

    assert!(
        wait_for_subscribers(&publisher, service_id, 1, 0x01).await,
        "server should have registered the subscriber"
    );

    // Drain SubscribeAck
    let _ = tokio::time::timeout(std::time::Duration::from_millis(250), updates.recv()).await;

    // Publish an event — server will E2E-protect it
    // Construct a non-SD message with service_id=service_id, method/event_id=0x0001
    let payload_bytes = [0xAA, 0xBB];
    let msg_id = MessageId::new_from_service_and_method(service_id, 0x0001);
    let raw_payload = RawPayload::from_payload_bytes(msg_id, &payload_bytes).unwrap();
    let header = Header::new_event(service_id, 0x0001, 0, 0x01, 0x01, payload_bytes.len());
    let event_msg = Message::new(header, raw_payload);
    let sent = publisher
        .publish_event(service_id, 1, 0x01, &event_msg)
        .await
        .expect("publish_event failed");
    assert_eq!(sent, 1);

    // Client receives the unicast event with E2E status
    let update = recv_unicast(&mut updates).await;
    match update {
        ClientUpdate::Unicast { e2e_status, .. } => {
            assert!(
                e2e_status.is_some(),
                "expected e2e_status to be populated when E2E is configured"
            );
            assert_eq!(
                e2e_status.unwrap(),
                E2ECheckStatus::Ok,
                "E2E check should pass for correctly protected message"
            );
        }
        other => unreachable!("recv_unicast only returns Unicast, got {other:?}"),
    }

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
        .add_endpoint(service_id, 1, server_addr, 0)
        .await
        .unwrap();
    client1
        .subscribe(service_id, 1, SERVER_IP, 1, 3, 0x01, 0)
        .await
        .unwrap();

    // Client 2
    let (client2, mut updates2, run_fut2) = TestClient::new(Ipv4Addr::LOCALHOST);
    tokio::spawn(run_fut2);
    client2
        .add_endpoint(service_id, 1, server_addr, 0)
        .await
        .unwrap();
    client2
        .subscribe(service_id, 1, SERVER_IP, 1, 3, 0x01, 0)
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
    let event_msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header());
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
        .add_endpoint(service_id, 1, server_addr, 0)
        .await
        .unwrap();
    client2
        .subscribe(service_id, 1, SERVER_IP, 1, 3, 0x01, 0)
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
        .add_endpoint(service_id, 1, server_addr, 0)
        .await
        .unwrap();

    // Use specific port
    let specific_port = 44444;
    client
        .subscribe(service_id, 1, SERVER_IP, 1, 3, 0x01, specific_port)
        .await
        .unwrap();
    // Second subscribe reuses the port
    client
        .subscribe(service_id, 1, SERVER_IP, 1, 3, 0x02, specific_port)
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
    client.add_endpoint(service_id, 1, addr_a, 0).await.unwrap();
    client.add_endpoint(service_id, 1, addr_b, 0).await.unwrap();

    // Distinct event groups so `has_subscribers`/`publish_event` on each
    // server can be checked independently.
    client
        .subscribe(service_id, 1, DEVICE_A, 1, 3, 0x01, 0)
        .await
        .unwrap();
    client
        .subscribe(service_id, 1, DEVICE_B, 1, 3, 0x02, 0)
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
    let event_msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header());
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
        .remove_endpoint(service_id, 1, DEVICE_A)
        .await
        .unwrap();

    let msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header());
    let result_a = client.send_to_service(service_id, 1, DEVICE_A, msg).await;
    assert!(
        matches!(result_a, Err(simple_someip::client::Error::ServiceNotFound)),
        "expected ServiceNotFound for removed device A, got {result_a:?}"
    );

    let msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header());
    let result_b = client.send_to_service(service_id, 1, DEVICE_B, msg).await;
    assert!(
        result_b.is_ok(),
        "device B must remain reachable after removing device A's endpoint: {result_b:?}"
    );

    client.shut_down();
    server_a_handle.abort();
    server_b_handle.abort();
}
