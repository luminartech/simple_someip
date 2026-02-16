//! Event publishing functionality

use super::subscription_manager::SubscriptionManager;
use crate::protocol::{Header, Message, MessageType, MessageTypeField, ReturnCode};
use crate::traits::{PayloadWireFormat, WireFormat};
use crate::Error;
use std::sync::Arc;
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
    pub async fn publish_raw_event(
        &self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
        event_id: u16,
        session_id: u32,
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
        let header = Header {
            message_id: crate::protocol::MessageId::new_from_service_and_method(
                service_id,
                event_id,
            ),
            length: super::someip_length(payload.len()),
            session_id,
            protocol_version,
            interface_version,
            message_type: MessageTypeField::new(MessageType::Notification, false),
            return_code: ReturnCode::Ok,
        };

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
    #[tokio::test]
    async fn test_event_publisher_creation() {
        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let socket = Arc::new(
            UdpSocket::bind("127.0.0.1:0")
                .await
                .expect("Failed to bind socket"),
        );

        let publisher = EventPublisher::new(subscriptions, socket);
        // Just test that it was created successfully
        assert!(std::mem::size_of_val(&publisher) > 0);
    }
}
