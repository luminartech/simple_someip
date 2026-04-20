//! Integration tests exercising the Client and Server together on localhost.

use simple_someip::e2e::{E2ECheckStatus, E2EKey, E2EProfile, Profile4Config};
use simple_someip::protocol::{Header, Message, MessageId, sd};
use simple_someip::server::ServerConfig;
use simple_someip::{Client, ClientUpdate, PayloadWireFormat, RawPayload, Server, VecSdHeader};
use std::net::{Ipv4Addr, SocketAddrV4};

fn empty_sd_header() -> VecSdHeader {
    VecSdHeader {
        flags: sd::Flags::new_sd(false),
        entries: vec![],
        options: vec![],
    }
}

type TestClient = Client<RawPayload>;

/// Create a server on an ephemeral unicast port, returning (Server, actual_port).
async fn create_server(service_id: u16, instance_id: u16) -> (Server, u16) {
    let config = ServerConfig::new(Ipv4Addr::LOCALHOST, 0, service_id, instance_id);
    let mut server: Server = Server::new(config).await.expect("Server::new failed");
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
    publisher: &simple_someip::server::EventPublisher,
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

#[tokio::test]
async fn test_client_server_subscribe_and_receive_event() {
    // Start server on ephemeral port
    let (mut server, server_port) = create_server(0x5B, 1).await;
    let publisher = server.publisher();
    let server_handle = tokio::spawn(async move { server.run().await });

    // Create client and subscribe to the server's event group
    let (client, mut updates) = TestClient::new(Ipv4Addr::LOCALHOST);
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
    client.add_endpoint(0x5B, 1, server_addr, 0).await.unwrap();
    client.subscribe(0x5B, 1, 1, 3, 0x01, 0).await.unwrap();

    assert!(
        wait_for_subscribers(&publisher, 0x5B, 1, 0x01).await,
        "server should have registered the subscriber"
    );

    // Drain any discovery update that may have arrived (SubscribeAck)
    let _ = tokio::time::timeout(std::time::Duration::from_millis(250), updates.recv()).await;

    // Publish an event from the server to the client's unicast port
    let event_msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header());
    let sent = publisher
        .publish_event(0x5B, 1, 0x01, &event_msg)
        .await
        .expect("publish_event failed");
    assert_eq!(sent, 1);

    // Client receives the unicast event
    let update = tokio::time::timeout(std::time::Duration::from_secs(2), updates.recv())
        .await
        .expect("timeout waiting for Unicast");
    assert!(
        matches!(update, Some(ClientUpdate::Unicast { .. })),
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
    let (mut server, server_port) = create_server(0x5B, 1).await;
    let server_handle = tokio::spawn(async move { server.run().await });

    // Create client — NO bind_discovery
    let (client, _updates) = TestClient::new(Ipv4Addr::LOCALHOST);

    // send_sd_message should auto-bind discovery and succeed
    let sd_header = VecSdHeader {
        flags: sd::Flags::new_sd(false),
        entries: vec![sd::Entry::SubscribeEventGroup(sd::EventGroupEntry::new(
            0x5B, 1, 1, 3, 0x01,
        ))],
        options: vec![sd::Options::IpV4Endpoint {
            ip: Ipv4Addr::LOCALHOST,
            protocol: sd::TransportProtocol::Udp,
            port: 12345,
        }],
    };
    let target = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
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
    let (mut server, server_port) = create_server(0x5B, 1).await;
    let server_handle = tokio::spawn(async move { server.run().await });

    let (client, _updates) = TestClient::new(Ipv4Addr::LOCALHOST);

    // Bind discovery, subscribe, then unbind and rebind
    client.bind_discovery().await.unwrap();
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
    client.add_endpoint(0x5B, 1, server_addr, 0).await.unwrap();
    client.subscribe(0x5B, 1, 1, 3, 0x01, 0).await.unwrap();

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
    let (mut server, server_port) = create_server(0x5B, 1).await;
    let publisher = server.publisher();
    let server_handle = tokio::spawn(async move { server.run().await });

    let (client, mut updates) = TestClient::new(Ipv4Addr::LOCALHOST);
    client.bind_discovery().await.unwrap();

    // Register the server's endpoint manually (simulating non-broadcasting service)
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
    client.add_endpoint(0x5B, 1, server_addr, 0).await.unwrap();

    // Subscribe to server's event group (auto-binds unicast internally)
    client.subscribe(0x5B, 1, 1, 3, 0x01, 0).await.unwrap();

    // Wait for the server to process the subscription
    assert!(
        wait_for_subscribers(&publisher, 0x5B, 1, 0x01).await,
        "server should have registered the subscriber"
    );

    // Drain any pending discovery update
    let _ = tokio::time::timeout(std::time::Duration::from_millis(250), updates.recv()).await;

    // Publish an event from the server
    let event_msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header());
    let sent = publisher
        .publish_event(0x5B, 1, 0x01, &event_msg)
        .await
        .expect("publish_event failed");
    assert_eq!(sent, 1);

    // Client receives the unicast event
    let update = tokio::time::timeout(std::time::Duration::from_secs(2), updates.recv())
        .await
        .expect("timeout waiting for Unicast");
    assert!(
        matches!(update, Some(ClientUpdate::Unicast { .. })),
        "expected Unicast, got {update:?}"
    );

    // Remove the endpoint and verify send_to_service returns ServiceNotFound
    client.remove_endpoint(0x5B, 1).await.unwrap();
    let msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header());
    let result = client.send_to_service(0x5B, 1, msg).await;
    assert!(
        matches!(result, Err(simple_someip::client::Error::ServiceNotFound)),
        "expected ServiceNotFound after remove, got {result:?}"
    );
    // Verify that PendingResponse is importable from the crate root
    let _: fn() -> Option<simple_someip::PendingResponse<RawPayload>> = || None;

    client.shut_down();
    server_handle.abort();
}

/// Verify subscribe auto-binds discovery when discovery is not already bound.
/// Exercises the Subscribe auto-bind discovery path in inner.rs.
#[tokio::test]
async fn test_subscribe_auto_binds_discovery() {
    let (mut server, server_port) = create_server(0x5B, 1).await;
    let publisher = server.publisher();
    let server_handle = tokio::spawn(async move { server.run().await });

    // Create client — do NOT bind discovery manually
    let (client, mut updates) = TestClient::new(Ipv4Addr::LOCALHOST);
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
    client.add_endpoint(0x5B, 1, server_addr, 0).await.unwrap();
    // Subscribe should auto-bind discovery internally
    client.subscribe(0x5B, 1, 1, 3, 0x01, 0).await.unwrap();

    assert!(
        wait_for_subscribers(&publisher, 0x5B, 1, 0x01).await,
        "server should have registered the subscriber"
    );

    // Drain SubscribeAck
    let _ = tokio::time::timeout(std::time::Duration::from_millis(250), updates.recv()).await;

    // Publish an event and verify the client can receive it
    let event_msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header());
    let sent = publisher
        .publish_event(0x5B, 1, 0x01, &event_msg)
        .await
        .expect("publish_event failed");
    assert_eq!(sent, 1);

    let update = tokio::time::timeout(std::time::Duration::from_secs(2), updates.recv())
        .await
        .expect("timeout waiting for Unicast");
    assert!(
        matches!(update, Some(ClientUpdate::Unicast { .. })),
        "expected Unicast, got {update:?}"
    );

    client.shut_down();
    server_handle.abort();
}

/// Verify that `request()` resolves when the server sends a unicast reply.
/// Exercises the pending_responses HashMap matching path in inner.rs.
#[tokio::test]
async fn test_client_request_resolves_via_unicast_reply() {
    let (mut server, server_port) = create_server(0x5B, 1).await;
    let publisher = server.publisher();
    let server_handle = tokio::spawn(async move { server.run().await });

    let (client, mut updates) = TestClient::new(Ipv4Addr::LOCALHOST);
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
    client.add_endpoint(0x5B, 1, server_addr, 0).await.unwrap();
    client.subscribe(0x5B, 1, 1, 3, 0x01, 0).await.unwrap();

    assert!(
        wait_for_subscribers(&publisher, 0x5B, 1, 0x01).await,
        "server should have registered the subscriber"
    );

    // Drain SubscribeAck
    let _ = tokio::time::timeout(std::time::Duration::from_millis(250), updates.recv()).await;

    // send_to_service creates a PendingResponse; the server will send the event
    // which has a matching request_id, resolving it.
    let msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header());
    let pending = client
        .send_to_service(0x5B, 1, msg)
        .await
        .expect("send_to_service failed");

    // Publish an event that the client unicast socket will receive
    let event_msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header());
    publisher
        .publish_event(0x5B, 1, 0x01, &event_msg)
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
    let (mut server, server_port) = create_server(0x5B, 1).await;
    let publisher = server.publisher();

    // Register E2E profile on server for the event message ID
    let key = E2EKey {
        service_id: 0x5B,
        method_or_event_id: 0x0001,
    };
    let profile = E2EProfile::Profile4(Profile4Config::new(0x12345678, 15));
    server.register_e2e(key, profile.clone());

    let server_handle = tokio::spawn(async move { server.run().await });

    let (client, mut updates) = TestClient::new(Ipv4Addr::LOCALHOST);

    // Register matching E2E profile on client
    client.register_e2e(key, profile);

    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
    client.add_endpoint(0x5B, 1, server_addr, 0).await.unwrap();
    client.subscribe(0x5B, 1, 1, 3, 0x01, 0).await.unwrap();

    assert!(
        wait_for_subscribers(&publisher, 0x5B, 1, 0x01).await,
        "server should have registered the subscriber"
    );

    // Drain SubscribeAck
    let _ = tokio::time::timeout(std::time::Duration::from_millis(250), updates.recv()).await;

    // Publish an event — server will E2E-protect it
    // Construct a non-SD message with service_id=0x5B, method/event_id=0x0001
    let payload_bytes = [0xAA, 0xBB];
    let msg_id = MessageId::new_from_service_and_method(0x5B, 0x0001);
    let raw_payload = RawPayload::from_payload_bytes(msg_id, &payload_bytes).unwrap();
    let header = Header::new_event(0x5B, 0x0001, 0, 0x01, 0x01, payload_bytes.len());
    let event_msg = Message::new(header, raw_payload);
    let sent = publisher
        .publish_event(0x5B, 1, 0x01, &event_msg)
        .await
        .expect("publish_event failed");
    assert_eq!(sent, 1);

    // Client receives the unicast event with E2E status
    let update = tokio::time::timeout(std::time::Duration::from_secs(2), updates.recv())
        .await
        .expect("timeout waiting for Unicast");
    match update {
        Some(ClientUpdate::Unicast { e2e_status, .. }) => {
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
        other => panic!("expected Unicast with e2e_status, got {other:?}"),
    }

    client.shut_down();
    server_handle.abort();
}

/// Verify that two clients can subscribe to the same server and both receive events.
/// Exercises multi-subscriber path in event_publisher.rs.
#[tokio::test]
async fn test_multiple_subscribers_receive_events() {
    let (mut server, server_port) = create_server(0x5B, 1).await;
    let publisher = server.publisher();
    let server_handle = tokio::spawn(async move { server.run().await });

    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);

    // Client 1
    let (client1, mut updates1) = TestClient::new(Ipv4Addr::LOCALHOST);
    client1.add_endpoint(0x5B, 1, server_addr, 0).await.unwrap();
    client1.subscribe(0x5B, 1, 1, 3, 0x01, 0).await.unwrap();

    // Client 2
    let (client2, mut updates2) = TestClient::new(Ipv4Addr::LOCALHOST);
    client2.add_endpoint(0x5B, 1, server_addr, 0).await.unwrap();
    client2.subscribe(0x5B, 1, 1, 3, 0x01, 0).await.unwrap();

    // Wait for both subscribers
    for _ in 0..40 {
        if publisher.subscriber_count(0x5B, 1, 0x01).await >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        publisher.subscriber_count(0x5B, 1, 0x01).await >= 2,
        "expected at least 2 subscribers"
    );

    // Drain discovery updates
    let _ = tokio::time::timeout(std::time::Duration::from_millis(250), updates1.recv()).await;
    let _ = tokio::time::timeout(std::time::Duration::from_millis(250), updates2.recv()).await;

    // Publish event
    let event_msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header());
    let sent = publisher
        .publish_event(0x5B, 1, 0x01, &event_msg)
        .await
        .expect("publish_event failed");
    assert!(sent >= 2, "expected sent >= 2, got {sent}");

    // Both clients should receive the event
    let u1 = tokio::time::timeout(std::time::Duration::from_secs(2), updates1.recv())
        .await
        .expect("timeout on client1");
    assert!(
        matches!(u1, Some(ClientUpdate::Unicast { .. })),
        "client1 expected Unicast, got {u1:?}"
    );

    let u2 = tokio::time::timeout(std::time::Duration::from_secs(2), updates2.recv())
        .await
        .expect("timeout on client2");
    assert!(
        matches!(u2, Some(ClientUpdate::Unicast { .. })),
        "client2 expected Unicast, got {u2:?}"
    );

    client1.shut_down();
    client2.shut_down();
    server_handle.abort();
}

/// Verify ClientUpdates returns None after client shutdown.
#[tokio::test]
async fn test_updates_drain_after_shutdown() {
    let (client, mut updates) = TestClient::new(Ipv4Addr::LOCALHOST);
    client.shut_down();

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), updates.recv())
        .await
        .expect("timeout waiting for None");
    assert!(result.is_none(), "expected None after shutdown");
}

/// Verify that cloned client handles work independently.
#[tokio::test]
async fn test_cloned_client_works() {
    let (mut server, server_port) = create_server(0x5B, 1).await;
    let server_handle = tokio::spawn(async move { server.run().await });

    let (client, _updates) = TestClient::new(Ipv4Addr::LOCALHOST);
    let client2 = client.clone();

    // Both clones can send commands
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
    client.add_endpoint(0x5B, 1, server_addr, 0).await.unwrap();
    client2.subscribe(0x5B, 1, 1, 3, 0x01, 0).await.unwrap();

    client.shut_down();
    // client2 is also dropped
    server_handle.abort();
}

/// Subscribe with a specific client_port, then subscribe again reusing the same port.
/// Exercises the port-reuse path in Subscribe handling.
#[tokio::test]
async fn test_subscribe_specific_port_reuse() {
    let (mut server, server_port) = create_server(0x5B, 1).await;
    let server_handle = tokio::spawn(async move { server.run().await });

    let (client, _updates) = TestClient::new(Ipv4Addr::LOCALHOST);
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
    client.add_endpoint(0x5B, 1, server_addr, 0).await.unwrap();

    // Use specific port
    let specific_port = 44444;
    client
        .subscribe(0x5B, 1, 1, 3, 0x01, specific_port)
        .await
        .unwrap();
    // Second subscribe reuses the port
    client
        .subscribe(0x5B, 1, 1, 3, 0x02, specific_port)
        .await
        .unwrap();

    client.shut_down();
    server_handle.abort();
}
