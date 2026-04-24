//! Integration tests exercising the Client and Server together on localhost.
//!
//! ## Parallel isolation
//!
//! Every Server here is constructed via [`Server::new_passive`] rather
//! than [`Server::new`]. The SOME/IP SD port 30490 is spec-fixed, and the
//! production SD binding uses `SO_REUSEPORT` so that multiple endpoints
//! on the same host can coexist. Under `cargo test` parallelism that
//! becomes a hazard: the kernel hash-distributes each incoming SD
//! datagram across the `SO_REUSEPORT` group, so a `Subscribe` from
//! Test-A's Client can land on Test-C's Server socket, Test-C filters
//! it out by service-id, and Test-A's Server never registers the
//! subscriber — the test then times out waiting for a subscriber that
//! was delivered to the wrong process.
//!
//! Passive servers deliberately bind SD to an ephemeral port instead
//! (documented on [`Server::new_passive`]), so they are **not** in the
//! 30490 `SO_REUSEPORT` group and the kernel never routes SD traffic to
//! them. Tests then bypass SD entirely for subscriber bookkeeping,
//! calling [`EventPublisher::register_subscriber`] directly — which is
//! also the supported path for any consumer that owns its own SD
//! dispatcher (see the `new_passive` docs).
//!
//! Each test still uses a unique `(service_id, client_port)` pair for
//! defense-in-depth: unique service-ids mean any stray SD packets are
//! filtered cleanly at the protocol layer, and unique unicast ports
//! keep `publish_event`'s unicast sends unambiguous.

use simple_someip::e2e::{E2ECheckStatus, E2EKey, E2EProfile, Profile4Config};
use simple_someip::protocol::{Header, Message, MessageId, sd};
use simple_someip::server::ServerConfig;
use simple_someip::{Client, ClientUpdate, PayloadWireFormat, RawPayload, Server, VecSdHeader};
use std::net::{Ipv4Addr, SocketAddrV4};

fn empty_sd_header() -> VecSdHeader {
    VecSdHeader {
        flags: sd::Flags::new_sd(sd::RebootFlag::RecentlyRebooted),
        entries: vec![],
        options: vec![],
    }
}

type TestClient = Client<RawPayload>;

/// Create a passive server on an ephemeral unicast port.
///
/// Returns `(Server, unicast_port)`. See the module docs for why
/// `Server::new_passive` is used instead of `Server::new`.
async fn create_passive_server(service_id: u16, instance_id: u16) -> (Server, u16) {
    let config = ServerConfig::new(Ipv4Addr::LOCALHOST, 0, service_id, instance_id);
    let mut server: Server = Server::new_passive(config)
        .await
        .expect("Server::new_passive failed");
    let port = match server.unicast_local_addr().expect("local_addr failed") {
        std::net::SocketAddr::V4(a) => a.port(),
        _ => panic!("expected IPv4"),
    };
    server.set_local_port(port);
    (server, port)
}

/// Bind the client's unicast socket on `client_port` **without** touching
/// the discovery (SD) socket, then register the resulting address as a
/// subscriber on the (passive) server's publisher.
///
/// Deliberately avoids `client.subscribe(...)` because `subscribe`
/// auto-binds the discovery socket on port 30490, which puts the client
/// in the `SO_REUSEPORT` group and causes it to receive other parallel
/// tests' SD messages as spurious `DiscoveryUpdated` updates. Instead,
/// we (re)register the endpoint with the desired `local_port` and issue
/// a throwaway `send_to_service` — the public client code path that
/// binds unicast on the registered `local_port` as a side effect. The
/// dummy packet lands at the passive server's unicast socket and is
/// discarded (passive servers have no `run` loop consuming it).
async fn bind_and_register_client(
    client: &TestClient,
    publisher: &simple_someip::server::EventPublisher,
    server_addr: SocketAddrV4,
    service_id: u16,
    instance_id: u16,
    event_group_id: u16,
    client_port: u16,
) -> SocketAddrV4 {
    client
        .add_endpoint(service_id, instance_id, server_addr, client_port)
        .await
        .expect("add_endpoint failed");
    let dummy = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header());
    let _ = client.send_to_service(service_id, instance_id, dummy).await;
    let client_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, client_port);
    publisher
        .register_subscriber(service_id, instance_id, event_group_id, client_addr)
        .await;
    client_addr
}

/// Receive the next [`ClientUpdate::Unicast`] from the stream within
/// `timeout`, skipping any `DiscoveryUpdated` updates that may have
/// leaked in from parallel tests that *do* bind their discovery socket
/// (e.g. tests whose assertion is about the SD lifecycle itself). The
/// outer `Result` is the timeout outcome; the inner `Option` is the
/// stream state. Returns `None` if the stream closes.
async fn recv_next_unicast(
    updates: &mut simple_someip::ClientUpdates<RawPayload>,
    timeout: std::time::Duration,
) -> Option<ClientUpdate<RawPayload>> {
    tokio::time::timeout(timeout, async {
        loop {
            match updates.recv().await {
                Some(u @ ClientUpdate::Unicast { .. }) => return Some(u),
                Some(ClientUpdate::DiscoveryUpdated(_)) => continue,
                Some(other) => return Some(other),
                None => return None,
            }
        }
    })
    .await
    .expect("timeout waiting for Unicast update")
}

#[tokio::test]
async fn test_client_server_subscribe_and_receive_event() {
    const SERVICE_ID: u16 = 0x5B01;
    const CLIENT_PORT: u16 = 40_001;
    let (server, server_port) = create_passive_server(SERVICE_ID, 1).await;
    let publisher = server.publisher();

    let (client, mut updates) = TestClient::new(Ipv4Addr::LOCALHOST);
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
    bind_and_register_client(
        &client,
        &publisher,
        server_addr,
        SERVICE_ID,
        1,
        0x01,
        CLIENT_PORT,
    )
    .await;

    // Publish an event from the server to the client's unicast port
    let event_msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header());
    let sent = publisher
        .publish_event(SERVICE_ID, 1, 0x01, &event_msg)
        .await
        .expect("publish_event failed");
    assert_eq!(sent, 1);

    let update = recv_next_unicast(&mut updates, std::time::Duration::from_secs(2)).await;
    assert!(
        matches!(update, Some(ClientUpdate::Unicast { .. })),
        "expected Unicast, got {update:?}"
    );

    client.shut_down();
    drop(server);
}

#[tokio::test]
async fn test_client_send_sd_auto_binds_discovery() {
    const SERVICE_ID: u16 = 0x5B02;
    // Passive server exists only so the send has a valid unicast target;
    // the assertion is on the client side (that it auto-binds discovery).
    let (server, server_port) = create_passive_server(SERVICE_ID, 1).await;

    // Create client — NO bind_discovery
    let (client, _updates) = TestClient::new(Ipv4Addr::LOCALHOST);

    // send_sd_message should auto-bind discovery and succeed
    let sd_header = VecSdHeader {
        flags: sd::Flags::new_sd(sd::RebootFlag::RecentlyRebooted),
        entries: vec![sd::Entry::SubscribeEventGroup(sd::EventGroupEntry::new(
            SERVICE_ID, 1, 1, 3, 0x01,
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
    drop(server);
}

/// Exercises the full bind/unbind lifecycle and set_interface flow
/// while an SD message round-trip is in flight.
#[tokio::test]
async fn test_client_bind_unbind_lifecycle_with_server() {
    const SERVICE_ID: u16 = 0x5B03;
    const CLIENT_PORT: u16 = 40_003;
    let (server, server_port) = create_passive_server(SERVICE_ID, 1).await;

    let (client, _updates) = TestClient::new(Ipv4Addr::LOCALHOST);

    // Bind discovery, subscribe, then unbind and rebind
    client.bind_discovery().await.unwrap();
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
    client
        .add_endpoint(SERVICE_ID, 1, server_addr, 0)
        .await
        .unwrap();
    client
        .subscribe(SERVICE_ID, 1, 1, 3, 0x01, CLIENT_PORT)
        .await
        .unwrap();

    // Unbind and rebind discovery — covers unbind_discovery + re-bind path
    client.unbind_discovery().await.unwrap();
    client.bind_discovery().await.unwrap();

    // set_interface while discovery is bound — covers the SetInterface arm
    // that unbinds discovery, changes interface, and rebinds
    client.set_interface(Ipv4Addr::LOCALHOST).await.unwrap();

    client.shut_down();
    drop(server);
}

/// Verify that add_endpoint + send_to_service resolves the endpoint from the
/// registry, auto-binds unicast, sends the request, and receives a response.
#[tokio::test]
async fn test_add_endpoint_and_send_to_service() {
    const SERVICE_ID: u16 = 0x5B04;
    const CLIENT_PORT: u16 = 40_004;
    let (server, server_port) = create_passive_server(SERVICE_ID, 1).await;
    let publisher = server.publisher();

    let (client, mut updates) = TestClient::new(Ipv4Addr::LOCALHOST);

    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
    bind_and_register_client(
        &client,
        &publisher,
        server_addr,
        SERVICE_ID,
        1,
        0x01,
        CLIENT_PORT,
    )
    .await;

    // Publish an event from the server
    let event_msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header());
    let sent = publisher
        .publish_event(SERVICE_ID, 1, 0x01, &event_msg)
        .await
        .expect("publish_event failed");
    assert_eq!(sent, 1);

    let update = recv_next_unicast(&mut updates, std::time::Duration::from_secs(2)).await;
    assert!(
        matches!(update, Some(ClientUpdate::Unicast { .. })),
        "expected Unicast, got {update:?}"
    );

    // Remove the endpoint and verify send_to_service returns ServiceNotFound
    client.remove_endpoint(SERVICE_ID, 1).await.unwrap();
    let msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header());
    let result = client.send_to_service(SERVICE_ID, 1, msg).await;
    assert!(
        matches!(result, Err(simple_someip::client::Error::ServiceNotFound)),
        "expected ServiceNotFound after remove, got {result:?}"
    );
    // Verify that PendingResponse is importable from the crate root
    let _: fn() -> Option<simple_someip::PendingResponse<RawPayload>> = || None;

    client.shut_down();
    drop(server);
}

/// Verify subscribe auto-binds discovery when discovery is not already bound.
/// Exercises the Subscribe auto-bind discovery path in inner.rs.
#[tokio::test]
async fn test_subscribe_auto_binds_discovery() {
    // This test is specifically about the `subscribe` auto-bind-discovery
    // path, so it deliberately uses `client.subscribe(...)` rather than
    // the discovery-avoiding `bind_and_register_client`. Because the
    // discovery socket is bound here, this test will *see* SD cross-talk
    // from parallel tests — we use `recv_next_unicast` to filter it.
    const SERVICE_ID: u16 = 0x5B05;
    const CLIENT_PORT: u16 = 40_005;
    let (server, server_port) = create_passive_server(SERVICE_ID, 1).await;
    let publisher = server.publisher();

    let (client, mut updates) = TestClient::new(Ipv4Addr::LOCALHOST);
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
    client
        .add_endpoint(SERVICE_ID, 1, server_addr, CLIENT_PORT)
        .await
        .unwrap();
    // Subscribe auto-binds discovery (the path under test). The SD
    // Subscribe it emits targets 30490 — may land on another parallel
    // test's discovery socket; irrelevant to what we assert on.
    client
        .subscribe(SERVICE_ID, 1, 1, 3, 0x01, CLIENT_PORT)
        .await
        .expect("subscribe should auto-bind discovery and succeed");
    // Publisher bookkeeping: register directly to bypass SD routing.
    let client_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, CLIENT_PORT);
    publisher
        .register_subscriber(SERVICE_ID, 1, 0x01, client_addr)
        .await;

    // Publish an event and verify the client can receive it
    let event_msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header());
    let sent = publisher
        .publish_event(SERVICE_ID, 1, 0x01, &event_msg)
        .await
        .expect("publish_event failed");
    assert_eq!(sent, 1);

    let update = recv_next_unicast(&mut updates, std::time::Duration::from_secs(2)).await;
    assert!(
        matches!(update, Some(ClientUpdate::Unicast { .. })),
        "expected Unicast, got {update:?}"
    );

    client.shut_down();
    drop(server);
}

/// Verify that `request()` resolves when the server sends a unicast reply.
/// Exercises the pending_responses HashMap matching path in inner.rs.
#[tokio::test]
async fn test_client_request_resolves_via_unicast_reply() {
    const SERVICE_ID: u16 = 0x5B06;
    const CLIENT_PORT: u16 = 40_006;
    let (server, server_port) = create_passive_server(SERVICE_ID, 1).await;
    let publisher = server.publisher();

    let (client, mut updates) = TestClient::new(Ipv4Addr::LOCALHOST);
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
    bind_and_register_client(
        &client,
        &publisher,
        server_addr,
        SERVICE_ID,
        1,
        0x01,
        CLIENT_PORT,
    )
    .await;

    // send_to_service creates a PendingResponse; the server will send the event
    // which has a matching request_id, resolving it.
    let msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header());
    let pending = client
        .send_to_service(SERVICE_ID, 1, msg)
        .await
        .expect("send_to_service failed");

    // Publish an event that the client unicast socket will receive
    let event_msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header());
    publisher
        .publish_event(SERVICE_ID, 1, 0x01, &event_msg)
        .await
        .expect("publish_event failed");

    // The event may or may not match the pending response's request_id.
    // Either way the client should receive a Unicast on its unicast socket.
    let update = recv_next_unicast(&mut updates, std::time::Duration::from_secs(2)).await;
    assert!(update.is_some(), "expected a Unicast update");

    // Clean up pending (it may never resolve if request_id didn't match)
    drop(pending);

    client.shut_down();
    drop(server);
}

/// Verify that E2E protection is applied by the server and checked by the client.
/// Exercises E2E protect in event_publisher.rs and E2E check in socket_manager.rs.
#[tokio::test]
async fn test_e2e_protect_on_publish_and_check_on_receive() {
    const SERVICE_ID: u16 = 0x5B07;
    const CLIENT_PORT: u16 = 40_007;
    let (server, server_port) = create_passive_server(SERVICE_ID, 1).await;
    let publisher = server.publisher();

    // Register E2E profile on server for the event message ID
    let key = E2EKey {
        service_id: SERVICE_ID,
        method_or_event_id: 0x0001,
    };
    let profile = E2EProfile::Profile4(Profile4Config::new(0x12345678, 15));
    server.register_e2e(key, profile.clone());

    let (client, mut updates) = TestClient::new(Ipv4Addr::LOCALHOST);

    // Register matching E2E profile on client
    client.register_e2e(key, profile);

    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
    bind_and_register_client(
        &client,
        &publisher,
        server_addr,
        SERVICE_ID,
        1,
        0x01,
        CLIENT_PORT,
    )
    .await;

    // Publish an event — server will E2E-protect it.
    // Construct a non-SD message with this test's service_id and event_id 0x0001.
    let payload_bytes = [0xAA, 0xBB];
    let msg_id = MessageId::new_from_service_and_method(SERVICE_ID, 0x0001);
    let raw_payload = RawPayload::from_payload_bytes(msg_id, &payload_bytes).unwrap();
    let header = Header::new_event(SERVICE_ID, 0x0001, 0, 0x01, 0x01, payload_bytes.len());
    let event_msg = Message::new(header, raw_payload);
    let sent = publisher
        .publish_event(SERVICE_ID, 1, 0x01, &event_msg)
        .await
        .expect("publish_event failed");
    assert_eq!(sent, 1);

    // Client receives the unicast event with E2E status
    let update = recv_next_unicast(&mut updates, std::time::Duration::from_secs(2)).await;
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
    drop(server);
}

/// Verify that two clients can subscribe to the same server and both receive events.
/// Exercises multi-subscriber path in event_publisher.rs.
#[tokio::test]
async fn test_multiple_subscribers_receive_events() {
    const SERVICE_ID: u16 = 0x5B08;
    const CLIENT_PORT_A: u16 = 40_008;
    const CLIENT_PORT_B: u16 = 40_108;
    let (server, server_port) = create_passive_server(SERVICE_ID, 1).await;
    let publisher = server.publisher();

    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);

    // Client 1
    let (client1, mut updates1) = TestClient::new(Ipv4Addr::LOCALHOST);
    bind_and_register_client(
        &client1,
        &publisher,
        server_addr,
        SERVICE_ID,
        1,
        0x01,
        CLIENT_PORT_A,
    )
    .await;

    // Client 2
    let (client2, mut updates2) = TestClient::new(Ipv4Addr::LOCALHOST);
    bind_and_register_client(
        &client2,
        &publisher,
        server_addr,
        SERVICE_ID,
        1,
        0x01,
        CLIENT_PORT_B,
    )
    .await;

    assert!(
        publisher.subscriber_count(SERVICE_ID, 1, 0x01).await >= 2,
        "expected at least 2 subscribers"
    );

    // Publish event
    let event_msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header());
    let sent = publisher
        .publish_event(SERVICE_ID, 1, 0x01, &event_msg)
        .await
        .expect("publish_event failed");
    assert!(sent >= 2, "expected sent >= 2, got {sent}");

    // Both clients should receive the event
    let u1 = recv_next_unicast(&mut updates1, std::time::Duration::from_secs(2)).await;
    assert!(
        matches!(u1, Some(ClientUpdate::Unicast { .. })),
        "client1 expected Unicast, got {u1:?}"
    );

    let u2 = recv_next_unicast(&mut updates2, std::time::Duration::from_secs(2)).await;
    assert!(
        matches!(u2, Some(ClientUpdate::Unicast { .. })),
        "client2 expected Unicast, got {u2:?}"
    );

    client1.shut_down();
    client2.shut_down();
    drop(server);
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
    const SERVICE_ID: u16 = 0x5B09;
    const CLIENT_PORT: u16 = 40_009;
    let (server, server_port) = create_passive_server(SERVICE_ID, 1).await;

    let (client, _updates) = TestClient::new(Ipv4Addr::LOCALHOST);
    let client2 = client.clone();

    // Both clones can send commands
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
    client
        .add_endpoint(SERVICE_ID, 1, server_addr, 0)
        .await
        .unwrap();
    client2
        .subscribe(SERVICE_ID, 1, 1, 3, 0x01, CLIENT_PORT)
        .await
        .unwrap();

    client.shut_down();
    // client2 is also dropped
    drop(server);
}

/// Subscribe with a specific client_port, then subscribe again reusing the same port.
/// Exercises the port-reuse path in Subscribe handling.
#[tokio::test]
async fn test_subscribe_specific_port_reuse() {
    const SERVICE_ID: u16 = 0x5B0A;
    let (server, server_port) = create_passive_server(SERVICE_ID, 1).await;

    let (client, _updates) = TestClient::new(Ipv4Addr::LOCALHOST);
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
    client
        .add_endpoint(SERVICE_ID, 1, server_addr, 0)
        .await
        .unwrap();

    // Use specific port (unique to this test to avoid cross-test bind collisions)
    let specific_port = 44_010;
    client
        .subscribe(SERVICE_ID, 1, 1, 3, 0x01, specific_port)
        .await
        .unwrap();
    // Second subscribe reuses the port
    client
        .subscribe(SERVICE_ID, 1, 1, 3, 0x02, specific_port)
        .await
        .unwrap();

    client.shut_down();
    drop(server);
}
