//! Manages event group subscriptions

use super::service_info::Subscriber;
use std::collections::HashMap;
use std::net::SocketAddrV4;

/// Manages subscriptions to event groups
#[derive(Debug)]
pub struct SubscriptionManager {
    /// Map of (service_id, instance_id, event_group_id) -> list of subscribers
    subscriptions: HashMap<(u16, u16, u16), Vec<Subscriber>>,
}

impl SubscriptionManager {
    /// Create a new subscription manager
    pub fn new() -> Self {
        Self {
            subscriptions: HashMap::new(),
        }
    }

    /// Add a subscriber to an event group
    pub fn subscribe(
        &mut self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
        subscriber_addr: SocketAddrV4,
    ) {
        let key = (service_id, instance_id, event_group_id);
        let subscribers = self.subscriptions.entry(key).or_insert_with(Vec::new);

        // Deduplicate: if this address is already subscribed, just refresh (don't add again)
        if subscribers.iter().any(|s| s.address == subscriber_addr) {
            tracing::debug!(
                "Refreshed existing subscriber {} for service 0x{:04X}, instance {}, event group 0x{:04X}",
                subscriber_addr,
                service_id,
                instance_id,
                event_group_id
            );
            return;
        }

        let subscriber = Subscriber::new(subscriber_addr, service_id, instance_id, event_group_id);
        subscribers.push(subscriber);

        tracing::info!(
            "New subscriber {} for service 0x{:04X}, instance {}, event group 0x{:04X}",
            subscriber_addr,
            service_id,
            instance_id,
            event_group_id
        );
    }

    /// Remove a subscriber from an event group
    pub fn unsubscribe(
        &mut self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
        subscriber_addr: SocketAddrV4,
    ) {
        let key = (service_id, instance_id, event_group_id);

        if let Some(subscribers) = self.subscriptions.get_mut(&key) {
            subscribers.retain(|s| s.address != subscriber_addr);

            if subscribers.is_empty() {
                self.subscriptions.remove(&key);
            }

            tracing::info!(
                "Removed subscriber {} from service 0x{:04X}, instance {}, event group 0x{:04X}",
                subscriber_addr,
                service_id,
                instance_id,
                event_group_id
            );
        }
    }

    /// Get all subscribers for an event group
    pub fn get_subscribers(
        &self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
    ) -> Vec<Subscriber> {
        let key = (service_id, instance_id, event_group_id);
        self.subscriptions
            .get(&key)
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    /// Get total number of active subscriptions
    pub fn subscription_count(&self) -> usize {
        self.subscriptions.values().map(|v| v.len()).sum()
    }
}

impl Default for SubscriptionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn test_subscription_management() {
        let mut manager = SubscriptionManager::new();
        let addr = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 1), 8080);

        // Subscribe
        manager.subscribe(0x5B, 1, 0x01, addr);
        assert_eq!(manager.subscription_count(), 1);

        // Get subscribers
        let subscribers = manager.get_subscribers(0x5B, 1, 0x01);
        assert_eq!(subscribers.len(), 1);
        assert_eq!(subscribers[0].address, addr);

        // Unsubscribe
        manager.unsubscribe(0x5B, 1, 0x01, addr);
        assert_eq!(manager.subscription_count(), 0);
    }
}
