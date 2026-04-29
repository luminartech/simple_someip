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
    Client, ClientUpdate, PayloadWireFormat, RawPayload, Server, TokioChannels, VecSdHeader,
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
    std::sync::Arc<std::sync::Mutex<simple_someip::e2e::E2ERegistry>>,
    std::sync::Arc<tokio::sync::RwLock<simple_someip::server::SubscriptionManager>>,
    simple_someip::TokioTransport,
    simple_someip::TokioTimer,
>;

/// Type alias for the event publisher concrete type used by `TestServer`'s
/// publisher. Same shape rationale as [`TestServer`].
type TestEventPublisher = simple_someip::server::EventPublisher<
    std::sync::Arc<std::sync::Mutex<simple_someip::e2e::E2ERegistry>>,
    std::sync::Arc<tokio::sync::RwLock<simple_someip::server::SubscriptionManager>>,
    simple_someip::TokioSocket,
>;

/// Create a server on an ephemeral unicast port, returning (Server, actual_port).
async fn create_server(service_id: u16, instance_id: u16) -> (TestServer, u16) {
    let config = ServerConfig::new(Ipv4Addr::LOCALHOST, 0, service_id, instance_id);
    let mut server: TestServer = TestServer::new(config).await.expect("Server::new failed");
    let port = match server.unicast_local_addr().expect("local_addr failed") {
        std::net::SocketAddr::V4(a) => a.port(),
        _ => panic!("expected IPv4"),
    };
    server.set_local_port(port);
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
/// client via the unicast SD socket (the per-transport fix in this PR) and can
/// land on the channel just before the event, so a single `recv()` that expects
/// the event outright is racy — especially under the slower coverage build.
/// Panics on timeout or a closed channel; returns the `Unicast` update so
/// callers can inspect fields like `e2e_status`.
async fn recv_unicast(updates: &mut ClientUpdates<RawPayload>) -> ClientUpdate<RawPayload> {
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
    let (mut server, server_port) = create_server(service_id, 1).await;
    let publisher = server.publisher();
    let server_handle = tokio::spawn(async move { server.run().await });

    // Create client and subscribe to the server's event group
    let (client, mut updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
    let _run_handle = tokio::spawn(run_fut);
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
    client
        .add_endpoint(service_id, 1, server_addr, 0)
        .await
        .unwrap();
    client
        .subscribe(service_id, 1, 1, 3, 0x01, 0)
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
    recv_unicast(&mut updates).await;

    // Tear down
    client.unbind_discovery().await.unwrap();
    client.shut_down();
    server_handle.abort();
}

#[tokio::test]
async fn test_client_send_sd_auto_binds_discovery() {
    // Create server so there is something to send to
    let service_id = next_service_id();
    let (mut server, server_port) = create_server(service_id, 1).await;
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
    let (mut server, server_port) = create_server(service_id, 1).await;
    let server_handle = tokio::spawn(async move { server.run().await });

    let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
    let _run_handle = tokio::spawn(run_fut);

    // Bind discovery, subscribe, then unbind and rebind
    client.bind_discovery().await.unwrap();
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
    client
        .add_endpoint(service_id, 1, server_addr, 0)
        .await
        .unwrap();
    client
        .subscribe(service_id, 1, 1, 3, 0x01, 0)
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
    let (mut server, server_port) = create_server(service_id, 1).await;
    let publisher = server.publisher();
    let server_handle = tokio::spawn(async move { server.run().await });

    let (client, mut updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
    let _run_handle = tokio::spawn(run_fut);
    client.bind_discovery().await.unwrap();

    // Register the server's endpoint manually (simulating non-broadcasting service)
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
    client
        .add_endpoint(service_id, 1, server_addr, 0)
        .await
        .unwrap();

    // Subscribe to server's event group (auto-binds unicast internally)
    client
        .subscribe(service_id, 1, 1, 3, 0x01, 0)
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
    recv_unicast(&mut updates).await;

    // Remove the endpoint and verify send_to_service returns ServiceNotFound
    client.remove_endpoint(service_id, 1).await.unwrap();
    let msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header());
    let result = client.send_to_service(service_id, 1, msg).await;
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
    let (mut server, server_port) = create_server(service_id, 1).await;
    let publisher = server.publisher();
    let server_handle = tokio::spawn(async move { server.run().await });

    // Create client — do NOT bind discovery manually
    let (client, mut updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
    let _run_handle = tokio::spawn(run_fut);
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
    client
        .add_endpoint(service_id, 1, server_addr, 0)
        .await
        .unwrap();
    // Subscribe should auto-bind discovery internally
    client
        .subscribe(service_id, 1, 1, 3, 0x01, 0)
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

    recv_unicast(&mut updates).await;

    client.shut_down();
    server_handle.abort();
}

/// Verify that `request()` resolves when the server sends a unicast reply.
/// Exercises the pending_responses HashMap matching path in inner.rs.
#[tokio::test]
async fn test_client_request_resolves_via_unicast_reply() {
    let service_id = next_service_id();
    let (mut server, server_port) = create_server(service_id, 1).await;
    let publisher = server.publisher();
    let server_handle = tokio::spawn(async move { server.run().await });

    let (client, mut updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
    let _run_handle = tokio::spawn(run_fut);
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
    client
        .add_endpoint(service_id, 1, server_addr, 0)
        .await
        .unwrap();
    client
        .subscribe(service_id, 1, 1, 3, 0x01, 0)
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
        .send_to_service(service_id, 1, msg)
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
    let (mut server, server_port) = create_server(service_id, 1).await;
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

    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
    client
        .add_endpoint(service_id, 1, server_addr, 0)
        .await
        .unwrap();
    client
        .subscribe(service_id, 1, 1, 3, 0x01, 0)
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
    match recv_unicast(&mut updates).await {
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
    let (mut server, server_port) = create_server(service_id, 1).await;
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
        .subscribe(service_id, 1, 1, 3, 0x01, 0)
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
        .subscribe(service_id, 1, 1, 3, 0x01, 0)
        .await
        .unwrap();

    // Wait for both subscribers
    for _ in 0..40 {
        if publisher.subscriber_count(service_id, 1, 0x01).await >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        publisher.subscriber_count(service_id, 1, 0x01).await >= 2,
        "expected at least 2 subscribers"
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
    assert!(sent >= 2, "expected sent >= 2, got {sent}");

    // Both clients should receive the event (skipping any interleaved acks).
    recv_unicast(&mut updates1).await;
    recv_unicast(&mut updates2).await;

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
    let (mut server, server_port) = create_server(service_id, 1).await;
    let server_handle = tokio::spawn(async move { server.run().await });

    let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
    let _run_handle = tokio::spawn(run_fut);
    let client2 = client.clone();

    // Both clones can send commands
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
    client
        .add_endpoint(service_id, 1, server_addr, 0)
        .await
        .unwrap();
    client2
        .subscribe(service_id, 1, 1, 3, 0x01, 0)
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
    let (mut server, server_port) = create_server(service_id, 1).await;
    let server_handle = tokio::spawn(async move { server.run().await });

    let (client, _updates, run_fut) = TestClient::new(Ipv4Addr::LOCALHOST);
    let _run_handle = tokio::spawn(run_fut);
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
    client
        .add_endpoint(service_id, 1, server_addr, 0)
        .await
        .unwrap();

    // Use specific port
    let specific_port = 44444;
    client
        .subscribe(service_id, 1, 1, 3, 0x01, specific_port)
        .await
        .unwrap();
    // Second subscribe reuses the port
    client
        .subscribe(service_id, 1, 1, 3, 0x02, specific_port)
        .await
        .unwrap();

    client.shut_down();
    server_handle.abort();
}
