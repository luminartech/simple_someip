use core::net::SocketAddrV4;
use heapless::index_map::FnvIndexMap;

/// Maximum number of service-endpoint entries the registry can track.
/// Must be a power of two ([`FnvIndexMap`] requirement). A real
/// vehicle-side SOME/IP deployment typically tracks at most a few dozen
/// services per ECU, so 64 is generous; bare-metal callers wanting a
/// tighter cap can fork. The cap exists so the registry is heap-free
/// (`heapless::FnvIndexMap` stores entries inline).
pub const SERVICE_REGISTRY_CAP: usize =
    crate::from_env_or(option_env!("SIMPLE_SOMEIP_SERVICE_REGISTRY_CAP"), 64);

/// Identifies one service instance ON A SPECIFIC DEVICE. The device IP is part
/// of the key because a fixed (ECU-Extract) instance id is shared by every
/// device on the subnet — keying without the source IP would collapse all of
/// them onto one entry (last-writer-wins). Mirrors how `SessionTracker` keys
/// per device by address.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ServiceEndpointKey {
    pub service_id: u16,
    pub instance_id: u16,
    pub source_ip: core::net::Ipv4Addr,
}

#[derive(Clone, Debug)]
pub struct ServiceEndpointInfo {
    pub addr: SocketAddrV4,
    pub local_port: u16,
    #[allow(dead_code)]
    pub major_version: u8,
    #[allow(dead_code)]
    pub minor_version: u32,
}

#[derive(Debug, Default)]
pub struct ServiceRegistry {
    endpoints: FnvIndexMap<ServiceEndpointKey, ServiceEndpointInfo, SERVICE_REGISTRY_CAP>,
}

/// Returned by [`ServiceRegistry::insert`] when the registry is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceRegistryFull;

impl ServiceRegistry {
    /// Insert or replace the endpoint for `key`. Returns `Ok(())` whether
    /// a previous value was replaced or this is a fresh entry. Returns
    /// `Err(ServiceRegistryFull)` if the registry is at
    /// [`SERVICE_REGISTRY_CAP`] and `key` is not already present.
    pub fn insert(
        &mut self,
        key: ServiceEndpointKey,
        info: ServiceEndpointInfo,
    ) -> Result<(), ServiceRegistryFull> {
        self.endpoints
            .insert(key, info)
            .map(|_| ())
            .map_err(|_| ServiceRegistryFull)
    }

    pub fn remove(&mut self, key: ServiceEndpointKey) -> Option<ServiceEndpointInfo> {
        self.endpoints.swap_remove(&key)
    }

    pub fn get(&self, key: ServiceEndpointKey) -> Option<&ServiceEndpointInfo> {
        self.endpoints.get(&key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::net::Ipv4Addr;

    fn key(service: u16, instance: u16, ip: Ipv4Addr) -> ServiceEndpointKey {
        ServiceEndpointKey {
            service_id: service,
            instance_id: instance,
            source_ip: ip,
        }
    }
    fn info(ip: Ipv4Addr, port: u16) -> ServiceEndpointInfo {
        ServiceEndpointInfo {
            addr: SocketAddrV4::new(ip, port),
            local_port: 0,
            major_version: 1,
            minor_version: 0,
        }
    }
    const A: Ipv4Addr = Ipv4Addr::LOCALHOST;
    const B: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 2);

    #[test]
    fn insert_and_get() {
        let mut reg = ServiceRegistry::default();
        reg.insert(key(0x47, 54, A), info(A, 30000)).unwrap();
        assert_eq!(reg.get(key(0x47, 54, A)).unwrap().addr.port(), 30000);
    }

    #[test]
    fn two_devices_same_service_instance_coexist() {
        // The regression: identical (service, instance) from different device IPs
        // must both persist and resolve independently.
        let mut reg = ServiceRegistry::default();
        reg.insert(key(0x47, 54, A), info(A, 30001)).unwrap();
        reg.insert(key(0x47, 54, B), info(B, 30002)).unwrap();
        assert_eq!(
            reg.get(key(0x47, 54, A)).unwrap().addr,
            SocketAddrV4::new(A, 30001)
        );
        assert_eq!(
            reg.get(key(0x47, 54, B)).unwrap().addr,
            SocketAddrV4::new(B, 30002)
        );
    }

    #[test]
    fn reinsert_same_key_replaces_in_place() {
        let mut reg = ServiceRegistry::default();
        reg.insert(key(0x47, 54, A), info(A, 30000)).unwrap();
        reg.insert(key(0x47, 54, A), info(A, 40000)).unwrap();
        assert_eq!(reg.get(key(0x47, 54, A)).unwrap().addr.port(), 40000);
    }

    #[test]
    fn remove_one_device_leaves_the_other() {
        // Regression for "StopOffer from one device evicted all".
        let mut reg = ServiceRegistry::default();
        reg.insert(key(0x47, 54, A), info(A, 30001)).unwrap();
        reg.insert(key(0x47, 54, B), info(B, 30002)).unwrap();
        assert!(reg.remove(key(0x47, 54, A)).is_some());
        assert!(reg.get(key(0x47, 54, A)).is_none());
        assert!(reg.get(key(0x47, 54, B)).is_some());
    }

    #[test]
    fn get_missing_returns_none() {
        let reg = ServiceRegistry::default();
        assert!(reg.get(key(0xFFFF, 0xFFFF, A)).is_none());
    }

    #[test]
    fn remove_missing_returns_none() {
        let mut reg = ServiceRegistry::default();
        assert!(reg.remove(key(0xFFFF, 0xFFFF, A)).is_none());
    }

    #[test]
    fn insert_returns_full_at_cap() {
        let mut reg = ServiceRegistry::default();
        for i in 0..SERVICE_REGISTRY_CAP {
            #[allow(clippy::cast_possible_truncation)]
            let k = key(i as u16, 0, A);
            assert!(reg.insert(k, info(A, 0)).is_ok());
        }
        assert_eq!(
            reg.insert(key(0xFFFF, 0xFFFF, A), info(A, 0)),
            Err(ServiceRegistryFull)
        );
    }

    #[test]
    fn insert_at_cap_for_existing_key_succeeds() {
        let mut reg = ServiceRegistry::default();
        for i in 0..SERVICE_REGISTRY_CAP {
            #[allow(clippy::cast_possible_truncation)]
            let k = key(i as u16, 0, A);
            assert!(reg.insert(k, info(A, 0)).is_ok());
        }
        assert!(reg.insert(key(0, 0, A), info(A, 9999)).is_ok());
        assert_eq!(reg.get(key(0, 0, A)).unwrap().addr.port(), 9999);
    }
}
