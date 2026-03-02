//! Service and event group information

use std::{net::SocketAddrV4, prelude::rust_2024::*};

/// Information about a SOME/IP service being provided
#[derive(Debug, Clone)]
pub struct ServiceInfo {
    /// Service ID
    pub service_id: u16,
    /// Instance ID
    pub instance_id: u16,
    /// Major version
    pub major_version: u8,
    /// Minor version
    pub minor_version: u32,
    /// Event groups this service provides
    pub event_groups: Vec<EventGroupInfo>,
}

/// Information about an event group
#[derive(Debug, Clone)]
pub struct EventGroupInfo {
    /// Event group ID
    pub event_group_id: u16,
    /// Events in this group (event IDs)
    pub event_ids: Vec<u16>,
}

impl EventGroupInfo {
    /// Create a new event group
    #[must_use]
    pub fn new(event_group_id: u16, event_ids: Vec<u16>) -> Self {
        Self {
            event_group_id,
            event_ids,
        }
    }
}

/// A subscriber to an event group
#[derive(Debug, Clone)]
pub struct Subscriber {
    /// Remote address of the subscriber
    pub address: SocketAddrV4,
    /// Event group they're subscribed to
    pub event_group_id: u16,
    /// Service ID
    pub service_id: u16,
    /// Instance ID
    pub instance_id: u16,
}

impl Subscriber {
    /// Create a new subscriber
    pub fn new(
        address: SocketAddrV4,
        service_id: u16,
        instance_id: u16,
        event_group_id: u16,
    ) -> Self {
        Self {
            address,
            event_group_id,
            service_id,
            instance_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::vec;

    #[test]
    fn test_event_group_info_new() {
        let info = EventGroupInfo::new(0x01, vec![0x8001, 0x8002]);
        assert_eq!(info.event_group_id, 0x01);
        assert_eq!(info.event_ids, vec![0x8001, 0x8002]);
    }

    #[test]
    fn test_subscriber_new() {
        let addr = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 100), 30500);
        let subscriber = Subscriber::new(addr, 0x5B, 1, 0x01);
        assert_eq!(subscriber.address, addr);
        assert_eq!(subscriber.service_id, 0x5B);
        assert_eq!(subscriber.instance_id, 1);
        assert_eq!(subscriber.event_group_id, 0x01);
    }
}
