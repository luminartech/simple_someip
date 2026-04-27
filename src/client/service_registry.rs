use core::net::SocketAddrV4;
use heapless::index_map::FnvIndexMap;

/// Maximum number of service-endpoint entries the registry can track.
/// Must be a power of two ([`FnvIndexMap`] requirement). A real
/// vehicle-side SOME/IP deployment typically tracks at most a few dozen
/// services per ECU, so 32 is generous; bare-metal callers wanting a
/// tighter cap can fork. The cap exists so the registry is heap-free
/// (`heapless::FnvIndexMap` stores entries inline).
pub const SERVICE_REGISTRY_CAP: usize = 32;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ServiceInstanceId {
    pub service_id: u16,
    pub instance_id: u16,
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
    endpoints: FnvIndexMap<ServiceInstanceId, ServiceEndpointInfo, SERVICE_REGISTRY_CAP>,
}

/// Returned by [`ServiceRegistry::insert`] when the registry is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceRegistryFull;

impl ServiceRegistry {
    /// Insert or replace the endpoint for `id`. Returns `Ok(())` whether
    /// a previous value was replaced or this is a fresh entry. Returns
    /// `Err(ServiceRegistryFull)` if the registry is at
    /// [`SERVICE_REGISTRY_CAP`] and `id` is not already present.
    pub fn insert(
        &mut self,
        id: ServiceInstanceId,
        info: ServiceEndpointInfo,
    ) -> Result<(), ServiceRegistryFull> {
        self.endpoints
            .insert(id, info)
            .map(|_| ())
            .map_err(|_| ServiceRegistryFull)
    }

    pub fn remove(&mut self, id: ServiceInstanceId) -> Option<ServiceEndpointInfo> {
        self.endpoints.swap_remove(&id)
    }

    pub fn get(&self, id: ServiceInstanceId) -> Option<&ServiceEndpointInfo> {
        self.endpoints.get(&id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::net::Ipv4Addr;

    fn test_id(service: u16, instance: u16) -> ServiceInstanceId {
        ServiceInstanceId {
            service_id: service,
            instance_id: instance,
        }
    }

    fn test_info(port: u16) -> ServiceEndpointInfo {
        ServiceEndpointInfo {
            addr: SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 1), port),
            local_port: 0,
            major_version: 1,
            minor_version: 0,
        }
    }

    #[test]
    fn insert_and_get() {
        let mut reg = ServiceRegistry::default();
        let id = test_id(0x1234, 0x0001);
        reg.insert(id, test_info(30000)).unwrap();
        let info = reg.get(id).unwrap();
        assert_eq!(info.addr.port(), 30000);
        assert_eq!(info.major_version, 1);
    }

    #[test]
    fn remove_returns_info() {
        let mut reg = ServiceRegistry::default();
        let id = test_id(0x1234, 0x0001);
        reg.insert(id, test_info(30000)).unwrap();
        let removed = reg.remove(id).unwrap();
        assert_eq!(removed.addr.port(), 30000);
        assert!(reg.get(id).is_none());
    }

    #[test]
    fn overwrite_replaces_info() {
        let mut reg = ServiceRegistry::default();
        let id = test_id(0x1234, 0x0001);
        reg.insert(id, test_info(30000)).unwrap();
        reg.insert(id, test_info(40000)).unwrap();
        assert_eq!(reg.get(id).unwrap().addr.port(), 40000);
    }

    #[test]
    fn get_missing_returns_none() {
        let reg = ServiceRegistry::default();
        assert!(reg.get(test_id(0xFFFF, 0xFFFF)).is_none());
    }

    #[test]
    fn remove_missing_returns_none() {
        let mut reg = ServiceRegistry::default();
        assert!(reg.remove(test_id(0xFFFF, 0xFFFF)).is_none());
    }

    #[test]
    fn insert_returns_full_at_cap() {
        let mut reg = ServiceRegistry::default();
        for i in 0..SERVICE_REGISTRY_CAP {
            #[allow(clippy::cast_possible_truncation)]
            let id = test_id(i as u16, 0);
            assert!(reg.insert(id, test_info(0)).is_ok());
        }
        let overflow_id = test_id(0xFFFF, 0xFFFF);
        assert_eq!(
            reg.insert(overflow_id, test_info(0)),
            Err(ServiceRegistryFull),
        );
    }

    #[test]
    fn insert_at_cap_for_existing_key_succeeds() {
        let mut reg = ServiceRegistry::default();
        for i in 0..SERVICE_REGISTRY_CAP {
            #[allow(clippy::cast_possible_truncation)]
            let id = test_id(i as u16, 0);
            assert!(reg.insert(id, test_info(0)).is_ok());
        }
        // Re-inserting an existing key replaces and does not require new
        // capacity.
        let existing = test_id(0, 0);
        assert!(reg.insert(existing, test_info(9999)).is_ok());
        assert_eq!(reg.get(existing).unwrap().addr.port(), 9999);
    }
}
