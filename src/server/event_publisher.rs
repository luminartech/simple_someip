//! Event publishing functionality

use super::Error;
use super::subscription_manager::SubscriptionManager;
use crate::protocol::{Header, Message};
use crate::traits::{PayloadWireFormat, WireFormat};
use std::{prelude::rust_2024::*, sync::Arc};
use tokio::net::UdpSocket;
use tokio::sync::RwLock;

/// Publishes events to subscribers
pub struct EventPublisher {
    subscriptions: Arc<RwLock<SubscriptionManager>>,
    socket: Arc<UdpSocket>,
}

impl EventPublisher {
    /// Create a new event publisher
    pub fn new(subscriptions: Arc<RwLock<SubscriptionManager>>, socket: Arc<UdpSocket>) -> Self {
        Self {
            subscriptions,
            socket,
        }
    }

    /// Publish an event to all subscribers of an event group
    ///
    /// # Arguments
    /// * `service_id` - Service ID
    /// * `instance_id` - Instance ID
    /// * `event_group_id` - Event group ID
    /// * `message` - The SOME/IP message to send (must be a notification/event)
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

        // Serialize the message once
        let mut buffer = Vec::new();
        message.encode(&mut buffer)?;

        // Send to all subscribers
        let mut sent_count = 0;
        for subscriber in &subscribers {
            match self.socket.send_to(&buffer, subscriber.address).await {
                Ok(_) => {
                    sent_count += 1;
                    tracing::trace!(
                        "Sent event to subscriber {} ({} bytes)",
                        subscriber.address,
                        buffer.len()
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

        // Serialize header + payload
        let mut buffer = Vec::new();
        header.encode(&mut buffer)?;
        buffer.extend_from_slice(payload);

        // Send to all subscribers
        let mut sent_count = 0;
        for subscriber in &subscribers {
            match self.socket.send_to(&buffer, subscriber.address).await {
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
    use crate::protocol::sd;
    use crate::traits::DiscoveryOnlyPayload;
    use std::net::{Ipv4Addr, SocketAddrV4};

    async fn make_publisher(
        subscriptions: Arc<RwLock<SubscriptionManager>>,
    ) -> (EventPublisher, Arc<UdpSocket>) {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let publisher = EventPublisher::new(subscriptions, Arc::clone(&socket));
        (publisher, socket)
    }

    fn make_test_message() -> Message<DiscoveryOnlyPayload> {
        let sd_hdr = sd::Header::new_find_services(false, &[]);
        Message::new_sd(0x0001, &sd_hdr)
    }

    #[tokio::test]
    async fn test_event_publisher_creation() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let socket = Arc::new(
            UdpSocket::bind("127.0.0.1:0")
                .await
                .expect("Failed to bind socket"),
        );

        let publisher = EventPublisher::new(subscriptions, socket);
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
            mgr.subscribe(0x5B, 1, 0x01, recv_addr);
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
    async fn test_publish_raw_event_with_subscriber() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));

        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let recv_addr = match receiver.local_addr().unwrap() {
            std::net::SocketAddr::V4(a) => a,
            _ => panic!("expected v4"),
        };

        {
            let mut mgr = subscriptions.write().await;
            mgr.subscribe(0x5B, 1, 0x01, recv_addr);
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
            mgr.subscribe(0x5B, 1, 0x01, addr1);
            mgr.subscribe(0x5B, 1, 0x01, addr2);
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
            );
        }

        assert!(publisher.has_subscribers(0x5B, 1, 0x01).await);
    }
}
