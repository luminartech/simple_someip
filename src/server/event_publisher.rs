//! Event publishing functionality

use super::Error;
use super::subscription_manager::SubscriptionManager;
use crate::UDP_BUFFER_SIZE;
use crate::e2e::{E2EKey, E2ERegistry};
use crate::protocol::{Header, Message};
use crate::traits::{PayloadWireFormat, WireFormat};
use std::sync::{Arc, Mutex};
use tokio::net::UdpSocket;
use tokio::sync::RwLock;

/// Publishes events to subscribers
pub struct EventPublisher {
    subscriptions: Arc<RwLock<SubscriptionManager>>,
    socket: Arc<UdpSocket>,
    e2e_registry: Arc<Mutex<E2ERegistry>>,
}

impl EventPublisher {
    /// Create a new event publisher
    pub fn new(
        subscriptions: Arc<RwLock<SubscriptionManager>>,
        socket: Arc<UdpSocket>,
        e2e_registry: Arc<Mutex<E2ERegistry>>,
    ) -> Self {
        Self {
            subscriptions,
            socket,
            e2e_registry,
        }
    }

    /// Publish an event to all subscribers of an event group
    ///
    /// # Arguments
    /// * `service_id` - Service ID
    /// * `instance_id` - Instance ID
    /// * `event_group_id` - Event group ID
    /// * `message` - The SOME/IP message to send (must be a notification/event)
    ///
    /// # Errors
    ///
    /// Returns an error if the message fails to serialize.
    ///
    /// # Panics
    ///
    /// Panics if the E2E registry mutex is poisoned.
    pub async fn publish_event<P: PayloadWireFormat>(
        &self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
        message: &Message<P>,
    ) -> Result<usize, Error> {
        // Get subscribers
        let subscribers = {
            let mgr = self.subscriptions.read().await;
            mgr.get_subscribers(service_id, instance_id, event_group_id)
        };

        if subscribers.is_empty() {
            tracing::trace!(
                "No subscribers for service 0x{:04X}, instance {}, event group 0x{:04X}",
                service_id,
                instance_id,
                event_group_id
            );
            return Ok(0);
        }

        // Fail fast with the capacity error rather than letting
        // `encode_to_slice` report a less-actionable protocol I/O error
        // when it runs out of buffer. Matches the raw-event path below
        // and the client socket_manager path.
        let required_size = message.required_size();
        if required_size > UDP_BUFFER_SIZE {
            tracing::error!(
                "Message size ({} bytes) exceeds UDP_BUFFER_SIZE ({}); dropping publish",
                required_size,
                UDP_BUFFER_SIZE
            );
            return Err(Error::Capacity("udp_buffer"));
        }

        // Serialize the message into a stack buffer sized to MTU.
        let mut buffer = [0u8; UDP_BUFFER_SIZE];
        let mut message_length = message.encode_to_slice(&mut buffer)?;

        // Apply E2E protect if configured. The `protected` stack buffer is
        // disjoint from `buffer`, so we can read the unprotected payload
        // directly out of `buffer[16..]` without a separate copy.
        {
            let key = E2EKey::from_message_id(message.header().message_id());
            let mut registry = self
                .e2e_registry
                .lock()
                .expect("e2e registry lock poisoned");
            if registry.contains_key(&key) {
                let upper_header: [u8; 8] = buffer[8..16].try_into().expect("upper header slice");
                let mut protected = [0u8; UDP_BUFFER_SIZE];
                let result = registry.protect(
                    key,
                    &buffer[16..message_length],
                    upper_header,
                    &mut protected,
                );
                match result {
                    Some(Ok(protected_len)) => {
                        if 16 + protected_len > UDP_BUFFER_SIZE {
                            tracing::error!(
                                "E2E-protected payload ({} bytes) exceeds UDP_BUFFER_SIZE ({}); \
                                 dropping publish",
                                16 + protected_len,
                                UDP_BUFFER_SIZE
                            );
                            return Err(Error::Capacity("udp_buffer"));
                        }
                        #[allow(clippy::cast_possible_truncation)]
                        let new_length: u32 = 8 + protected_len as u32;
                        buffer[4..8].copy_from_slice(&new_length.to_be_bytes());
                        buffer[16..16 + protected_len].copy_from_slice(&protected[..protected_len]);
                        message_length = 16 + protected_len;
                    }
                    Some(Err(e)) => {
                        tracing::error!("E2E protect error: {:?}", e);
                    }
                    None => unreachable!("contains_key was true"),
                }
            }
        }

        let datagram = &buffer[..message_length];

        // Send to all subscribers
        let mut sent_count = 0;
        for subscriber in &subscribers {
            match self.socket.send_to(datagram, subscriber.address).await {
                Ok(_) => {
                    sent_count += 1;
                    tracing::trace!(
                        "Sent event to subscriber {} ({} bytes)",
                        subscriber.address,
                        message_length
                    );
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to send event to subscriber {}: {:?}",
                        subscriber.address,
                        e
                    );
                }
            }
        }

        tracing::debug!(
            "Published event to {}/{} subscribers for service 0x{:04X}",
            sent_count,
            subscribers.len(),
            service_id
        );

        Ok(sent_count)
    }

    /// Publish raw event data (already serialized with E2E protection)
    ///
    /// This is useful when you've already applied E2E protection to the payload
    ///
    /// # Errors
    ///
    /// Returns an error if the SOME/IP header fails to serialize.
    #[allow(clippy::too_many_arguments)]
    pub async fn publish_raw_event(
        &self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
        event_id: u16,
        request_id: u32,
        protocol_version: u8,
        interface_version: u8,
        payload: &[u8],
    ) -> Result<usize, Error> {
        // Get subscribers
        let subscribers = {
            let mgr = self.subscriptions.read().await;
            mgr.get_subscribers(service_id, instance_id, event_group_id)
        };

        if subscribers.is_empty() {
            return Ok(0);
        }

        // Build SOME/IP header
        let header = Header::new_event(
            service_id,
            event_id,
            request_id,
            protocol_version,
            interface_version,
            payload.len(),
        );

        // Serialize header + payload into a stack buffer sized to MTU.
        let mut buffer = [0u8; UDP_BUFFER_SIZE];
        let header_len = header.encode_to_slice(&mut buffer)?;
        let total_len = header_len + payload.len();
        if total_len > UDP_BUFFER_SIZE {
            tracing::error!(
                "raw event ({} bytes) exceeds UDP_BUFFER_SIZE ({}); dropping publish",
                total_len,
                UDP_BUFFER_SIZE
            );
            return Err(Error::Capacity("udp_buffer"));
        }
        buffer[header_len..total_len].copy_from_slice(payload);
        let datagram = &buffer[..total_len];

        // Send to all subscribers
        let mut sent_count = 0;
        for subscriber in &subscribers {
            match self.socket.send_to(datagram, subscriber.address).await {
                Ok(_) => {
                    sent_count += 1;
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to send raw event to {}: {:?}",
                        subscriber.address,
                        e
                    );
                }
            }
        }

        Ok(sent_count)
    }

    /// Check if there are any active subscribers for a specific event group
    ///
    /// # Arguments
    /// * `service_id` - Service ID
    /// * `instance_id` - Instance ID
    /// * `event_group_id` - Event group ID
    ///
    /// # Returns
    /// `true` if there are subscribers, `false` otherwise
    pub async fn has_subscribers(
        &self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
    ) -> bool {
        let mgr = self.subscriptions.read().await;
        !mgr.get_subscribers(service_id, instance_id, event_group_id)
            .is_empty()
    }

    /// Register a subscriber for an event group.
    ///
    /// This is useful when subscription handling is managed externally
    /// (e.g. by a client that shares the SD socket) rather than by the
    /// server's own `run()` loop.
    ///
    /// Calling this method with the same `(service_id, instance_id,
    /// event_group_id, subscriber_addr)` tuple is idempotent — the
    /// underlying [`SubscriptionManager`] deduplicates — so external
    /// dispatchers can safely call it on every incoming
    /// `SubscribeEventGroup` (including TTL refreshes) without growing
    /// the subscriber list.
    ///
    /// # TTL / expiration
    ///
    /// This method does **not** track the SOME/IP-SD Subscribe TTL.
    /// Subscribers registered here persist until explicitly removed via
    /// [`EventPublisher::remove_subscriber`] (or until the
    /// [`EventPublisher`] itself is dropped). External dispatchers are
    /// responsible for detecting stale subscriptions — for example, by
    /// tracking the last refresh time per subscriber and calling
    /// `remove_subscriber` when no refresh has arrived within the
    /// advertised TTL — otherwise subscribers accumulate for the
    /// lifetime of the process.
    ///
    /// # Errors
    ///
    /// Returns [`crate::server::SubscribeError`] when the underlying
    /// [`SubscriptionManager`] cannot record the subscription because a
    /// bounded capacity was hit:
    /// - `SubscribersPerGroupFull` — the per-event-group subscriber list
    ///   is full.
    /// - `EventGroupsFull` — the outer event-group map is full.
    ///
    /// On `Err`, the subscriber was **not** registered and no events
    /// will be delivered to `subscriber_addr` for this event group.
    /// External dispatchers should treat this the same way the server's
    /// own `run()` loop does: emit a `SubscribeNack` (or equivalent
    /// upstream notification) so the peer does not assume it is
    /// subscribed. A duplicate registration for an already-subscribed
    /// address returns `Ok(())` (deduplicated).
    pub async fn register_subscriber(
        &self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
        subscriber_addr: std::net::SocketAddrV4,
    ) -> Result<(), crate::server::SubscribeError> {
        let mut mgr = self.subscriptions.write().await;
        mgr.subscribe(service_id, instance_id, event_group_id, subscriber_addr)
    }

    /// Remove a previously-registered subscriber from an event group.
    ///
    /// Counterpart to [`EventPublisher::register_subscriber`] for
    /// externally managed subscriptions. Calling this method with an
    /// address that is not currently subscribed is a no-op.
    ///
    /// Intended for use by external SD dispatchers to clean up stale
    /// subscriptions whose TTL has expired or whose remote peer has
    /// rebooted. The server's own `run()` loop does not call this
    /// method; it is purely for external management.
    pub async fn remove_subscriber(
        &self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
        subscriber_addr: std::net::SocketAddrV4,
    ) {
        let mut mgr = self.subscriptions.write().await;
        mgr.unsubscribe(service_id, instance_id, event_group_id, subscriber_addr);
    }

    /// Get the current number of subscribers for a specific event group
    pub async fn subscriber_count(
        &self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
    ) -> usize {
        let mgr = self.subscriptions.read().await;
        mgr.get_subscribers(service_id, instance_id, event_group_id)
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::sd::test_support::{TestPayload, empty_sd_header};
    use std::net::{Ipv4Addr, SocketAddrV4};
    use std::vec;
    use std::vec::Vec;

    fn test_registry() -> Arc<Mutex<E2ERegistry>> {
        Arc::new(Mutex::new(E2ERegistry::new()))
    }

    async fn make_publisher(
        subscriptions: Arc<RwLock<SubscriptionManager>>,
    ) -> (EventPublisher, Arc<UdpSocket>) {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let publisher = EventPublisher::new(subscriptions, Arc::clone(&socket), test_registry());
        (publisher, socket)
    }

    fn make_test_message() -> Message<TestPayload> {
        Message::new_sd(0x0001, &empty_sd_header())
    }

    #[tokio::test]
    async fn test_event_publisher_creation() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let socket = Arc::new(
            UdpSocket::bind("127.0.0.1:0")
                .await
                .expect("Failed to bind socket"),
        );

        let publisher = EventPublisher::new(subscriptions, socket, test_registry());
        assert!(std::mem::size_of_val(&publisher) > 0);
    }

    #[tokio::test]
    async fn test_publish_event_no_subscribers() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let (publisher, _) = make_publisher(subscriptions).await;
        let msg = make_test_message();
        let count = publisher.publish_event(0x5B, 1, 0x01, &msg).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_publish_event_with_subscriber() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));

        // Create a receiver socket to act as subscriber
        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let recv_addr = match receiver.local_addr().unwrap() {
            std::net::SocketAddr::V4(a) => a,
            _ => panic!("expected v4"),
        };

        // Add subscriber
        {
            let mut mgr = subscriptions.write().await;
            mgr.subscribe(0x5B, 1, 0x01, recv_addr).unwrap();
        }

        let (publisher, _) = make_publisher(subscriptions).await;
        let msg = make_test_message();
        let count = publisher.publish_event(0x5B, 1, 0x01, &msg).await.unwrap();
        assert_eq!(count, 1);

        // Verify data was received
        let mut buf = [0u8; 1024];
        let (len, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            receiver.recv_from(&mut buf),
        )
        .await
        .expect("timeout receiving event")
        .unwrap();
        assert!(len > 0);
    }

    #[tokio::test]
    async fn test_publish_raw_event_no_subscribers() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let (publisher, _) = make_publisher(subscriptions).await;
        let count = publisher
            .publish_raw_event(0x5B, 1, 0x01, 0x8001, 0x0001, 0x01, 0x01, &[0xAA, 0xBB])
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_publish_raw_event_exceeds_udp_buffer_returns_capacity_error() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9999);
        {
            let mut mgr = subscriptions.write().await;
            mgr.subscribe(0x5B, 1, 0x01, addr);
        }
        let (publisher, _) = make_publisher(subscriptions).await;

        // Payload = UDP_BUFFER_SIZE forces total (header + payload) over the cap.
        let too_big = vec![0u8; UDP_BUFFER_SIZE];
        let err = publisher
            .publish_raw_event(0x5B, 1, 0x01, 0x8001, 0x0001, 0x01, 0x01, &too_big)
            .await
            .expect_err("oversize payload must error, not report Ok(0)");
        match err {
            Error::Capacity(tag) => assert_eq!(tag, "udp_buffer"),
            other => panic!("expected Error::Capacity(\"udp_buffer\"), got {other:?}"),
        }
    }

    /// Regression guard against 343da67: without the pre-check, an oversize
    /// message would fail with a less-actionable protocol I/O error from
    /// `encode_to_slice`'s slice writer running out of buffer, rather than
    /// the explicit `Error::Capacity("udp_buffer")` the new branch returns.
    ///
    /// Note: a subscriber must be registered first — the pre-check sits
    /// after the `subscribers.is_empty()` early return, so without one the
    /// function would return `Ok(0)` and never touch the new branch,
    /// giving a false positive.
    #[tokio::test]
    async fn publish_event_pre_encode_exceeds_udp_buffer_returns_capacity_error() {
        use crate::RawPayload;
        use crate::protocol::{Header, MessageId, MessageType, MessageTypeField, ReturnCode};

        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9999);
        {
            let mut mgr = subscriptions.write().await;
            mgr.subscribe(0x5B, 1, 0x01, addr);
        }
        let (publisher, _) = make_publisher(subscriptions).await;

        // 16-byte header + 1485-byte payload = 1501 bytes, one over the cap.
        // Mirrors the client-side oversize fixture in
        // `send_raw_message_exceeding_udp_buffer_returns_capacity_error`.
        let message_id = MessageId::new_from_service_and_method(0x1234, 0x5678);
        let payload_bytes = [0u8; 1485];
        let payload = RawPayload::from_payload_bytes(message_id, &payload_bytes).unwrap();
        let header = Header::new(
            message_id,
            0x0001_0001,
            0x01,
            0x01,
            MessageTypeField::new(MessageType::Request, false),
            ReturnCode::Ok,
            payload_bytes.len(),
        );
        let message = Message::new(header, payload);
        assert!(
            message.required_size() > UDP_BUFFER_SIZE,
            "fixture must exceed cap",
        );

        let err = publisher
            .publish_event(0x5B, 1, 0x01, &message)
            .await
            .expect_err("oversize message must error, not report Ok(_)");
        match err {
            Error::Capacity(tag) => assert_eq!(tag, "udp_buffer"),
            other => panic!("expected Error::Capacity(\"udp_buffer\"), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_publish_raw_event_with_subscriber() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));

        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let recv_addr = match receiver.local_addr().unwrap() {
            std::net::SocketAddr::V4(a) => a,
            _ => panic!("expected v4"),
        };

        {
            let mut mgr = subscriptions.write().await;
            mgr.subscribe(0x5B, 1, 0x01, recv_addr).unwrap();
        }

        let (publisher, _) = make_publisher(subscriptions).await;
        let payload = [0xDE, 0xAD];
        let count = publisher
            .publish_raw_event(0x5B, 1, 0x01, 0x8001, 0x0001, 0x01, 0x01, &payload)
            .await
            .unwrap();
        assert_eq!(count, 1);

        // Verify the received data contains a valid SOME/IP header + payload
        let mut buf = [0u8; 1024];
        let (len, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            receiver.recv_from(&mut buf),
        )
        .await
        .expect("timeout receiving raw event")
        .unwrap();
        // 16 bytes header + 2 bytes payload
        assert_eq!(len, 18);
        // Check payload at end
        assert_eq!(&buf[16..18], &payload);
    }

    #[tokio::test]
    async fn test_subscriber_count() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let addr1 = SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 9001);
        let addr2 = SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 9002);

        {
            let mut mgr = subscriptions.write().await;
            mgr.subscribe(0x5B, 1, 0x01, addr1).unwrap();
            mgr.subscribe(0x5B, 1, 0x01, addr2).unwrap();
        }

        let (publisher, _) = make_publisher(subscriptions).await;
        assert_eq!(publisher.subscriber_count(0x5B, 1, 0x01).await, 2);
    }

    #[tokio::test]
    async fn test_has_subscribers() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let (publisher, _) = make_publisher(Arc::clone(&subscriptions)).await;

        assert!(!publisher.has_subscribers(0x5B, 1, 0x01).await);

        {
            let mut mgr = subscriptions.write().await;
            mgr.subscribe(
                0x5B,
                1,
                0x01,
                SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 9001),
            )
            .unwrap();
        }

        assert!(publisher.has_subscribers(0x5B, 1, 0x01).await);
    }

    // ── register_subscriber / remove_subscriber ──────────────────────────
    //
    // These cover the externally-managed subscription path used by
    // clients that drive SD through their own discovery socket and
    // dispatch `SubscribeEventGroup` messages into an `EventPublisher`.

    const ADDR_A: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9001);
    const ADDR_B: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9002);
    const ADDR_C: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9003);

    #[tokio::test]
    async fn register_subscriber_adds_to_manager() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let (publisher, _) = make_publisher(Arc::clone(&subscriptions)).await;

        assert!(!publisher.has_subscribers(0x5B, 1, 0x01).await);
        publisher.register_subscriber(0x5B, 1, 0x01, ADDR_A).await.unwrap();
        assert!(publisher.has_subscribers(0x5B, 1, 0x01).await);
        assert_eq!(publisher.subscriber_count(0x5B, 1, 0x01).await, 1);
    }

    #[tokio::test]
    async fn register_subscriber_is_idempotent_on_repeat() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let (publisher, _) = make_publisher(Arc::clone(&subscriptions)).await;

        // Simulate TTL refreshes — the same (tuple, addr) called repeatedly
        // must not grow the subscriber list.
        publisher.register_subscriber(0x5B, 1, 0x01, ADDR_A).await.unwrap();
        publisher.register_subscriber(0x5B, 1, 0x01, ADDR_A).await.unwrap();
        publisher.register_subscriber(0x5B, 1, 0x01, ADDR_A).await.unwrap();

        assert_eq!(publisher.subscriber_count(0x5B, 1, 0x01).await, 1);
    }

    #[tokio::test]
    async fn register_subscriber_separates_different_eventgroups() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let (publisher, _) = make_publisher(Arc::clone(&subscriptions)).await;

        publisher.register_subscriber(0x5B, 1, 0x01, ADDR_A).await.unwrap();
        publisher.register_subscriber(0x5B, 1, 0x02, ADDR_A).await.unwrap();

        assert_eq!(publisher.subscriber_count(0x5B, 1, 0x01).await, 1);
        assert_eq!(publisher.subscriber_count(0x5B, 1, 0x02).await, 1);
        assert!(publisher.has_subscribers(0x5B, 1, 0x01).await);
        assert!(publisher.has_subscribers(0x5B, 1, 0x02).await);
    }

    #[tokio::test]
    async fn remove_subscriber_happy_path() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let (publisher, _) = make_publisher(Arc::clone(&subscriptions)).await;

        publisher.register_subscriber(0x5B, 1, 0x01, ADDR_A).await.unwrap();
        assert!(publisher.has_subscribers(0x5B, 1, 0x01).await);

        publisher.remove_subscriber(0x5B, 1, 0x01, ADDR_A).await;
        assert!(!publisher.has_subscribers(0x5B, 1, 0x01).await);
        assert_eq!(publisher.subscriber_count(0x5B, 1, 0x01).await, 0);
    }

    #[tokio::test]
    async fn remove_subscriber_leaves_siblings_alone() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let (publisher, _) = make_publisher(Arc::clone(&subscriptions)).await;

        publisher.register_subscriber(0x5B, 1, 0x01, ADDR_A).await.unwrap();
        publisher.register_subscriber(0x5B, 1, 0x01, ADDR_B).await.unwrap();
        publisher.register_subscriber(0x5B, 1, 0x01, ADDR_C).await.unwrap();
        assert_eq!(publisher.subscriber_count(0x5B, 1, 0x01).await, 3);

        publisher.remove_subscriber(0x5B, 1, 0x01, ADDR_B).await;
        assert_eq!(publisher.subscriber_count(0x5B, 1, 0x01).await, 2);

        // The remaining two are still in the list.
        let mgr = subscriptions.read().await;
        let subscribers = mgr.get_subscribers(0x5B, 1, 0x01);
        let addrs: Vec<_> = subscribers.iter().map(|s| s.address).collect();
        assert!(addrs.contains(&ADDR_A));
        assert!(addrs.contains(&ADDR_C));
        assert!(!addrs.contains(&ADDR_B));
    }

    #[tokio::test]
    async fn remove_subscriber_nonexistent_is_noop() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let (publisher, _) = make_publisher(Arc::clone(&subscriptions)).await;

        // Remove from an empty manager.
        publisher.remove_subscriber(0x5B, 1, 0x01, ADDR_A).await;
        assert_eq!(publisher.subscriber_count(0x5B, 1, 0x01).await, 0);

        // Register one subscriber, then remove a different address.
        publisher.register_subscriber(0x5B, 1, 0x01, ADDR_A).await.unwrap();
        publisher.remove_subscriber(0x5B, 1, 0x01, ADDR_B).await;
        assert_eq!(publisher.subscriber_count(0x5B, 1, 0x01).await, 1);

        // Remove with wrong service_id is also a no-op.
        publisher.remove_subscriber(0x99, 1, 0x01, ADDR_A).await;
        assert_eq!(publisher.subscriber_count(0x5B, 1, 0x01).await, 1);
    }

    #[tokio::test]
    async fn remove_subscriber_all_then_has_subscribers_false() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let (publisher, _) = make_publisher(Arc::clone(&subscriptions)).await;

        publisher.register_subscriber(0x5B, 1, 0x01, ADDR_A).await.unwrap();
        publisher.register_subscriber(0x5B, 1, 0x01, ADDR_B).await.unwrap();
        assert!(publisher.has_subscribers(0x5B, 1, 0x01).await);

        publisher.remove_subscriber(0x5B, 1, 0x01, ADDR_A).await;
        assert!(publisher.has_subscribers(0x5B, 1, 0x01).await);

        publisher.remove_subscriber(0x5B, 1, 0x01, ADDR_B).await;
        assert!(!publisher.has_subscribers(0x5B, 1, 0x01).await);
    }

    #[tokio::test]
    async fn register_and_remove_roundtrip_preserves_idempotence() {
        // Register → remove → register again should end with exactly one
        // subscriber; the remove in the middle should not leave ghost state.
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let (publisher, _) = make_publisher(Arc::clone(&subscriptions)).await;

        publisher.register_subscriber(0x5B, 1, 0x01, ADDR_A).await.unwrap();
        publisher.remove_subscriber(0x5B, 1, 0x01, ADDR_A).await;
        publisher.register_subscriber(0x5B, 1, 0x01, ADDR_A).await.unwrap();

        assert_eq!(publisher.subscriber_count(0x5B, 1, 0x01).await, 1);
    }
}
