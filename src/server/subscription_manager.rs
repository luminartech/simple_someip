//! Manages event group subscriptions

use super::service_info::Subscriber;
use core::future::Future;
use heapless::{Vec as HeaplessVec, index_map::FnvIndexMap};
#[cfg(feature = "server-tokio")]
use std::sync::Arc;
use std::{net::SocketAddrV4, vec::Vec};
#[cfg(feature = "server-tokio")]
use tokio::sync::RwLock;

/// Max number of distinct `(service_id, instance_id, event_group_id)` event
/// groups with active subscribers. Must be a power of two.
const EVENT_GROUPS_CAP: usize = 32;

/// Max number of subscribers per event group. Excess subscribers are dropped
/// with a `warn!` log rather than silently.
const SUBSCRIBERS_PER_GROUP: usize = 16;

// Compile-time invariants. Trip these at `cargo build` so that retuning
// the constants above can't quietly produce a `subscribe` impl that
// panics on first push (zero `SUBSCRIBERS_PER_GROUP`) or that fails the
// `heapless::FnvIndexMap` build (non-power-of-two `EVENT_GROUPS_CAP`).
const _: () = assert!(
    SUBSCRIBERS_PER_GROUP >= 1,
    "SUBSCRIBERS_PER_GROUP must be >= 1: a value of 0 would crash subscribe() on first push"
);
const _: () = assert!(
    EVENT_GROUPS_CAP.is_power_of_two(),
    "EVENT_GROUPS_CAP must be a power of two for heapless::FnvIndexMap"
);

/// Why a call to [`SubscriptionManager::subscribe`] failed to record a new
/// subscriber. Callers (typically the server's `Subscribe` handler) should
/// use this to emit a `SubscribeNack` instead of a misleading `SubscribeAck`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscribeError {
    /// The per-event-group subscriber list is already full
    /// (`SUBSCRIBERS_PER_GROUP` entries). The caller's request was not
    /// recorded.
    SubscribersPerGroupFull,
    /// The outer event-group map is already full (`EVENT_GROUPS_CAP`
    /// distinct `(service_id, instance_id, event_group_id)` keys). The
    /// caller's request was not recorded.
    EventGroupsFull,
}

impl core::fmt::Display for SubscribeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::SubscribersPerGroupFull => write!(
                f,
                "subscribers-per-group at capacity ({SUBSCRIBERS_PER_GROUP})"
            ),
            Self::EventGroupsFull => {
                write!(f, "event-group map at capacity ({EVENT_GROUPS_CAP})")
            }
        }
    }
}

type SubscribersList = HeaplessVec<Subscriber, SUBSCRIBERS_PER_GROUP>;

/// Manages subscriptions to event groups.
///
/// Capacity is bounded at compile time: up to `EVENT_GROUPS_CAP` distinct
/// event groups, each with up to `SUBSCRIBERS_PER_GROUP` subscribers.
#[derive(Debug)]
pub struct SubscriptionManager {
    /// Map of (`service_id`, `instance_id`, `event_group_id`) -> list of subscribers
    subscriptions: FnvIndexMap<(u16, u16, u16), SubscribersList, EVENT_GROUPS_CAP>,
}

impl SubscriptionManager {
    /// Create a new subscription manager
    #[must_use]
    pub fn new() -> Self {
        Self {
            subscriptions: FnvIndexMap::new(),
        }
    }

    /// Add a subscriber to an event group.
    ///
    /// Returns `Ok(())` both when a new subscriber is added and when the
    /// given `(service_id, instance_id, event_group_id, subscriber_addr)`
    /// is already subscribed — the call is idempotent / deduplicated, and
    /// no stored subscriber state is modified on a duplicate. There is no
    /// TTL bump or other refresh side-effect today; if TTL-refresh
    /// semantics are added later, this docstring and the duplicate-log
    /// wording will be updated together.
    ///
    /// Returns `Err(SubscribeError)` when the request could not be
    /// recorded because a bounded capacity was hit — the caller
    /// (typically the server's `Subscribe` handler) should send a
    /// `SubscribeNack` on `Err`, not a `SubscribeAck`.
    ///
    /// # Errors
    ///
    /// Returns:
    /// - `SubscribeError::SubscribersPerGroupFull` when an existing event
    ///   group already has `SUBSCRIBERS_PER_GROUP` subscribers and this
    ///   call would push a new one.
    /// - `SubscribeError::EventGroupsFull` when this is the first
    ///   subscriber for a previously-unseen `(service_id, instance_id,
    ///   event_group_id)` triple but the outer event-group map is full
    ///   (`EVENT_GROUPS_CAP` distinct groups).
    ///
    /// # Panics
    ///
    /// Panics if `SUBSCRIBERS_PER_GROUP == 0`, a compile-time constant that
    /// must be at least one for a newly-allocated subscriber list to accept
    /// its first entry.
    pub fn subscribe(
        &mut self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
        subscriber_addr: SocketAddrV4,
    ) -> Result<(), SubscribeError> {
        let key = (service_id, instance_id, event_group_id);

        if let Some(subscribers) = self.subscriptions.get_mut(&key) {
            // Deduplicate: if this address is already subscribed, skip adding
            // it again. No stored subscriber state is modified — the log
            // message reflects that. If real refresh semantics (e.g. TTL
            // bump on re-subscribe) are wanted later, update the per-
            // subscriber record here and rename the log accordingly.
            if subscribers.iter().any(|s| s.address == subscriber_addr) {
                tracing::debug!(
                    "Subscriber {} already subscribed for service 0x{:04X}, instance {}, \
                     event group 0x{:04X}; skipping duplicate",
                    subscriber_addr,
                    service_id,
                    instance_id,
                    event_group_id
                );
                return Ok(());
            }

            let subscriber =
                Subscriber::new(subscriber_addr, service_id, instance_id, event_group_id);
            if subscribers.push(subscriber).is_err() {
                tracing::warn!(
                    "Subscribers-per-group at capacity ({}); dropping new subscriber {} \
                     for service 0x{:04X}, instance {}, event group 0x{:04X}",
                    SUBSCRIBERS_PER_GROUP,
                    subscriber_addr,
                    service_id,
                    instance_id,
                    event_group_id
                );
                return Err(SubscribeError::SubscribersPerGroupFull);
            }

            tracing::info!(
                "Subscriber {} added for service 0x{:04X}, instance {}, event group 0x{:04X}",
                subscriber_addr,
                service_id,
                instance_id,
                event_group_id
            );
            return Ok(());
        }

        // New event group — allocate the list and insert.
        let mut list = SubscribersList::new();
        // The first push into an empty heapless::Vec cannot fail as long
        // as SUBSCRIBERS_PER_GROUP >= 1 (enforced by the constant's
        // definition). Use `expect` here — a future refactor setting the
        // cap to 0 would trip this at test time instead of silently
        // dropping the only subscriber for a new event group.
        list.push(Subscriber::new(
            subscriber_addr,
            service_id,
            instance_id,
            event_group_id,
        ))
        .expect(
            "new SubscribersList must accept the first subscriber; \
             SUBSCRIBERS_PER_GROUP must be >= 1",
        );

        if self.subscriptions.insert(key, list).is_err() {
            tracing::warn!(
                "Event-group map at capacity ({}); dropping subscriber {} for new group \
                 service 0x{:04X}, instance {}, event group 0x{:04X}",
                EVENT_GROUPS_CAP,
                subscriber_addr,
                service_id,
                instance_id,
                event_group_id
            );
            return Err(SubscribeError::EventGroupsFull);
        }

        tracing::info!(
            "Subscriber {} added for service 0x{:04X}, instance {}, event group 0x{:04X}",
            subscriber_addr,
            service_id,
            instance_id,
            event_group_id
        );
        Ok(())
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
    #[must_use]
    pub fn get_subscribers(
        &self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
    ) -> Vec<Subscriber> {
        let key = (service_id, instance_id, event_group_id);
        self.subscriptions
            .get(&key)
            .map(|list| list.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Get total number of active subscriptions
    #[must_use]
    pub fn subscription_count(&self) -> usize {
        self.subscriptions.values().map(|v| v.len()).sum()
    }
}

impl Default for SubscriptionManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared handle to the server's subscription table.
///
/// Abstracts over `Arc<RwLock<SubscriptionManager>>` on `std` and over
/// critical-section-backed equivalents on bare metal. The futures
/// returned by the methods are not required to be `Send`, allowing
/// single-threaded executors (embassy-style) to satisfy the trait
/// without an `Arc<RwLock>`-style shared state.
///
/// Both `Server` and `EventPublisher` clone the same handle at construction
/// time; the underlying subscription state is shared between them.
pub trait SubscriptionHandle: Clone + 'static {
    /// Add a subscriber to an event group.
    ///
    /// Idempotent: if the subscriber is already present, this is a no-op
    /// returning `Ok(())`. Returns `Err(SubscribeError)` if a capacity
    /// limit would be exceeded.
    fn subscribe(
        &self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
        subscriber_addr: SocketAddrV4,
    ) -> impl Future<Output = Result<(), SubscribeError>> + '_;

    /// Remove a subscriber from an event group.
    fn unsubscribe(
        &self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
        subscriber_addr: SocketAddrV4,
    ) -> impl Future<Output = ()> + '_;

    /// Visit each subscriber for the given event group with `f`.
    ///
    /// The implementation typically holds an internal read lock for the
    /// duration of the visit; `f` is a synchronous `FnMut` callback —
    /// the caller MUST NOT yield inside it. A common pattern is to copy
    /// the subscriber addresses into a stack-allocated buffer here, then
    /// release the lock and dispatch sends in a second phase.
    ///
    /// Returns the total number of subscribers visited. Replaces the
    /// previous `get_subscribers -> Vec<Subscriber>` API; the visitor
    /// pattern lets `EventPublisher::publish_event` avoid a per-event
    /// heap allocation.
    fn for_each_subscriber<'a, F>(
        &'a self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
        f: F,
    ) -> impl Future<Output = usize> + 'a
    where
        F: FnMut(&Subscriber) + 'a;
}

#[cfg(feature = "server-tokio")]
impl SubscriptionHandle for Arc<RwLock<SubscriptionManager>> {
    fn subscribe(
        &self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
        subscriber_addr: SocketAddrV4,
    ) -> impl Future<Output = Result<(), SubscribeError>> + '_ {
        let this = self.clone();
        async move {
            this.write()
                .await
                .subscribe(service_id, instance_id, event_group_id, subscriber_addr)
        }
    }

    fn unsubscribe(
        &self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
        subscriber_addr: SocketAddrV4,
    ) -> impl Future<Output = ()> + '_ {
        let this = self.clone();
        async move {
            this.write().await.unsubscribe(
                service_id,
                instance_id,
                event_group_id,
                subscriber_addr,
            );
        }
    }

    fn for_each_subscriber<'a, F>(
        &'a self,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
        mut f: F,
    ) -> impl Future<Output = usize> + 'a
    where
        F: FnMut(&Subscriber) + 'a,
    {
        let this = self.clone();
        async move {
            let guard = this.read().await;
            let key = (service_id, instance_id, event_group_id);
            match guard.subscriptions.get(&key) {
                Some(list) => {
                    for sub in list {
                        f(sub);
                    }
                    list.len()
                }
                None => 0,
            }
        }
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
        manager.subscribe(0x5B, 1, 0x01, addr).unwrap();
        assert_eq!(manager.subscription_count(), 1);

        // Get subscribers
        let subscribers = manager.get_subscribers(0x5B, 1, 0x01);
        assert_eq!(subscribers.len(), 1);
        assert_eq!(subscribers[0].address, addr);

        // Unsubscribe
        manager.unsubscribe(0x5B, 1, 0x01, addr);
        assert_eq!(manager.subscription_count(), 0);
    }

    #[test]
    fn test_duplicate_subscriber_refresh() {
        let mut manager = SubscriptionManager::new();
        let addr = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 1), 8080);

        manager.subscribe(0x5B, 1, 0x01, addr).unwrap();
        assert_eq!(manager.subscription_count(), 1);

        // Subscribe same address again — should deduplicate
        manager.subscribe(0x5B, 1, 0x01, addr).unwrap();
        assert_eq!(manager.subscription_count(), 1);
    }

    #[test]
    fn test_unsubscribe_nonexistent_key() {
        let mut manager = SubscriptionManager::new();
        let addr = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 9000);

        // Unsubscribe from empty manager — should not panic
        manager.unsubscribe(0x99, 1, 0x01, addr);
        assert_eq!(manager.subscription_count(), 0);
    }

    #[test]
    fn test_get_subscribers_empty() {
        let manager = SubscriptionManager::new();
        assert!(manager.get_subscribers(0x99, 1, 0x01).is_empty());
    }

    #[test]
    fn test_default_impl() {
        let manager = SubscriptionManager::default();
        assert_eq!(manager.subscription_count(), 0);
    }

    #[test]
    fn subscribers_per_group_capacity_overflow() {
        let mut manager = SubscriptionManager::new();
        // Fill one event group to capacity.
        for i in 0..SUBSCRIBERS_PER_GROUP {
            let addr =
                SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 8000 + u16::try_from(i).unwrap());
            manager.subscribe(0x5B, 1, 0x01, addr).unwrap();
        }
        assert_eq!(manager.subscription_count(), SUBSCRIBERS_PER_GROUP);

        // One more is dropped, and the call reports SubscribersPerGroupFull
        // so the server can NACK.
        let extra = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 9999);
        assert_eq!(
            manager.subscribe(0x5B, 1, 0x01, extra),
            Err(SubscribeError::SubscribersPerGroupFull),
        );
        assert_eq!(manager.subscription_count(), SUBSCRIBERS_PER_GROUP);
        // Extra subscriber should not appear in the list.
        let subs = manager.get_subscribers(0x5B, 1, 0x01);
        assert!(subs.iter().all(|s| s.address != extra));
    }

    #[test]
    fn event_groups_capacity_overflow() {
        let mut manager = SubscriptionManager::new();
        let addr = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 8000);
        // Fill the outer map to capacity with distinct event groups.
        for i in 0..EVENT_GROUPS_CAP {
            let eg = u16::try_from(i).unwrap();
            manager.subscribe(0x5B, 1, eg, addr).unwrap();
        }
        assert_eq!(manager.subscription_count(), EVENT_GROUPS_CAP);

        // A new event group beyond capacity is dropped, and the call reports
        // EventGroupsFull so the server can NACK.
        let overflow_eg = u16::try_from(EVENT_GROUPS_CAP).unwrap();
        assert_eq!(
            manager.subscribe(0x5B, 1, overflow_eg, addr),
            Err(SubscribeError::EventGroupsFull),
        );
        assert_eq!(manager.subscription_count(), EVENT_GROUPS_CAP);
        assert!(manager.get_subscribers(0x5B, 1, overflow_eg).is_empty());
    }
}
