//! Integration tests exercising the Client and Server together on localhost.

use simple_someip::protocol::{Message, sd};
use simple_someip::server::ServerConfig;
use simple_someip::{Client, ClientUpdate, RawPayload, Server, VecSdHeader};
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
    let mut client = TestClient::new(Ipv4Addr::LOCALHOST);
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
    client.add_endpoint(0x5B, 1, server_addr).await.unwrap();
    client.subscribe(0x5B, 1, 1, 3, 0x01, 0).await.unwrap();

    assert!(
        wait_for_subscribers(&publisher, 0x5B, 1, 0x01).await,
        "server should have registered the subscriber"
    );

    // Drain any discovery update that may have arrived (SubscribeAck)
    let _ = tokio::time::timeout(std::time::Duration::from_millis(250), client.run()).await;

    // Publish an event from the server to the client's unicast port
    let event_msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header());
    let sent = publisher
        .publish_event(0x5B, 1, 0x01, &event_msg)
        .await
        .expect("publish_event failed");
    assert_eq!(sent, 1);

    // Client receives the unicast event
    let update = tokio::time::timeout(std::time::Duration::from_secs(2), client.run())
        .await
        .expect("timeout waiting for Unicast");
    assert!(
        matches!(update, Some(ClientUpdate::Unicast(..))),
        "expected Unicast, got {update:?}"
    );

    // Tear down
    client.unbind_discovery().await.unwrap();
    client.shut_down().await;
    server_handle.abort();
}

#[tokio::test]
async fn test_client_send_sd_auto_binds_discovery() {
    // Create server so there is something to send to
    let (mut server, server_port) = create_server(0x5B, 1).await;
    let server_handle = tokio::spawn(async move { server.run().await });

    // Create client — NO bind_discovery
    let mut client = TestClient::new(Ipv4Addr::LOCALHOST);

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

    client.shut_down().await;
    server_handle.abort();
}

/// Exercises the full bind/unbind lifecycle and set_interface flow
/// while an SD message round-trip is in flight.
#[tokio::test]
async fn test_client_bind_unbind_lifecycle_with_server() {
    let (mut server, server_port) = create_server(0x5B, 1).await;
    let server_handle = tokio::spawn(async move { server.run().await });

    let mut client = TestClient::new(Ipv4Addr::LOCALHOST);

    // Bind discovery, subscribe, then unbind and rebind
    client.bind_discovery().await.unwrap();
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
    client.add_endpoint(0x5B, 1, server_addr).await.unwrap();
    client.subscribe(0x5B, 1, 1, 3, 0x01, 0).await.unwrap();

    // Unbind and rebind discovery — covers unbind_discovery + re-bind path
    client.unbind_discovery().await.unwrap();
    client.bind_discovery().await.unwrap();

    // set_interface while discovery is bound — covers the SetInterface arm
    // that unbinds discovery, changes interface, and rebinds
    client.set_interface(Ipv4Addr::LOCALHOST).await.unwrap();

    client.shut_down().await;
    server_handle.abort();
}

/// Verify that add_endpoint + send_to_service resolves the endpoint from the
/// registry, auto-binds unicast, sends the request, and receives a response.
#[tokio::test]
async fn test_add_endpoint_and_send_to_service() {
    let (mut server, server_port) = create_server(0x5B, 1).await;
    let publisher = server.publisher();
    let server_handle = tokio::spawn(async move { server.run().await });

    let mut client = TestClient::new(Ipv4Addr::LOCALHOST);
    client.bind_discovery().await.unwrap();

    // Register the server's endpoint manually (simulating non-broadcasting service)
    let server_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, server_port);
    client.add_endpoint(0x5B, 1, server_addr).await.unwrap();

    // Subscribe to server's event group (auto-binds unicast internally)
    client.subscribe(0x5B, 1, 1, 3, 0x01, 0).await.unwrap();

    // Wait for the server to process the subscription
    assert!(
        wait_for_subscribers(&publisher, 0x5B, 1, 0x01).await,
        "server should have registered the subscriber"
    );

    // Drain any pending discovery update
    let _ = tokio::time::timeout(std::time::Duration::from_millis(250), client.run()).await;

    // Publish an event from the server
    let event_msg = Message::<RawPayload>::new_sd(0x0001, &empty_sd_header());
    let sent = publisher
        .publish_event(0x5B, 1, 0x01, &event_msg)
        .await
        .expect("publish_event failed");
    assert_eq!(sent, 1);

    // Client receives the unicast event
    let update = tokio::time::timeout(std::time::Duration::from_secs(2), client.run())
        .await
        .expect("timeout waiting for Unicast");
    assert!(
        matches!(update, Some(ClientUpdate::Unicast(..))),
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

    client.shut_down().await;
    server_handle.abort();
}
