use crate::NetEndpoint;
use heapless::index_map::FnvIndexMap;

/// Maximum number of service-endpoint entries the registry can track.
/// Must be a power of two ([`FnvIndexMap`] requirement). A real
/// vehicle-side SOME/IP deployment typically tracks at most a few dozen
/// services per ECU, so 64 is generous; bare-metal callers wanting a
/// tighter cap can fork. The cap exists so the registry is heap-free
/// (`heapless::FnvIndexMap` stores entries inline).
pub const SERVICE_REGISTRY_CAP: usize =
    crate::from_env_or(option_env!("SIMPLE_SOMEIP_SERVICE_REGISTRY_CAP"), 64);

/// Identifies one service instance by its wire identity: the service id
/// plus the provider's transport endpoint. Per AUTOSAR `PRS_SOMEIP`
/// §4.2.1.3, a service instance is identified "through the combination
/// of the Service ID combined with the socket (i.e. IP-address,
/// transport protocol, and port number)". The instance id is NOT part
/// of the key — it never appears in the SOME/IP header
/// (`[PRS_SOMEIP_00162]`) and lives in [`ServiceEndpointInfo`] instead.
/// Co-hosted instances of the same service on one device are required
/// to use distinct ports (`[PRS_SOMEIP_00163]`), so the socket key
/// keeps them distinct.
///
/// This is both the client service-registry key and the public
/// addressing handle: [`Client::subscribe`](crate::Client::subscribe),
/// [`send_to_service`](crate::Client::send_to_service),
/// [`request`](crate::Client::request), and
/// [`remove_endpoint`](crate::Client::remove_endpoint) all take one.
/// Build it once per (service, endpoint) pair and reuse it — it is
/// `Copy`. For today's UDP transports, [`ServiceEndpointKey::udp`] is
/// the convenient constructor.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ServiceEndpointKey {
    pub service_id: u16,
    pub endpoint: NetEndpoint,
}

impl ServiceEndpointKey {
    #[must_use]
    pub const fn new(service_id: u16, endpoint: NetEndpoint) -> Self {
        Self {
            service_id,
            endpoint,
        }
    }

    /// Key for a UDP endpoint — the common case for SOME/IP transports.
    #[must_use]
    pub const fn udp(service_id: u16, addr: core::net::SocketAddr) -> Self {
        Self::new(service_id, NetEndpoint::udp(addr))
    }
}

#[derive(Clone, Debug)]
pub struct ServiceEndpointInfo {
    /// SOME/IP instance id offered at this endpoint. Not part of the
    /// key (`[PRS_SOMEIP_00162]`: instance ids discriminate instances
    /// of the same service only), but `SubscribeEventgroup` SD entries
    /// carry it on the wire, so it is stored as data.
    pub instance_id: u16,
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
    use core::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    fn key(service: u16, ip: Ipv4Addr, port: u16) -> ServiceEndpointKey {
        ServiceEndpointKey::udp(service, SocketAddr::V4(SocketAddrV4::new(ip, port)))
    }
    fn info(instance: u16) -> ServiceEndpointInfo {
        ServiceEndpointInfo {
            instance_id: instance,
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
        reg.insert(key(0x47, A, 30000), info(54)).unwrap();
        assert_eq!(reg.get(key(0x47, A, 30000)).unwrap().instance_id, 54);
    }

    #[test]
    fn two_devices_same_service_instance_coexist() {
        // The regression: identical (service, instance) from different device IPs
        // must both persist and resolve independently.
        let mut reg = ServiceRegistry::default();
        reg.insert(key(0x47, A, 30001), info(54)).unwrap();
        reg.insert(key(0x47, B, 30002), info(54)).unwrap();
        assert_eq!(reg.get(key(0x47, A, 30001)).unwrap().instance_id, 54);
        assert_eq!(reg.get(key(0x47, B, 30002)).unwrap().instance_id, 54);
    }

    #[test]
    fn same_service_same_ip_distinct_ports_are_distinct_entries() {
        // [PRS_SOMEIP_00163]: co-hosted instances of the same service
        // must use distinct ports — the socket key keeps them apart.
        let mut reg = ServiceRegistry::default();
        reg.insert(key(0x1234, A, 30509), info(1)).unwrap();
        reg.insert(key(0x1234, A, 30510), info(2)).unwrap();
        assert_eq!(reg.get(key(0x1234, A, 30509)).unwrap().instance_id, 1);
        assert_eq!(reg.get(key(0x1234, A, 30510)).unwrap().instance_id, 2);
    }

    #[test]
    fn same_socket_distinct_protocols_are_distinct_entries() {
        let mut reg = ServiceRegistry::default();
        let addr = SocketAddr::V4(SocketAddrV4::new(A, 30509));
        reg.insert(ServiceEndpointKey::udp(0x1234, addr), info(1))
            .unwrap();
        reg.insert(
            ServiceEndpointKey::new(0x1234, crate::NetEndpoint::tcp(addr)),
            info(2),
        )
        .unwrap();
        assert_eq!(
            reg.get(ServiceEndpointKey::udp(0x1234, addr))
                .unwrap()
                .instance_id,
            1
        );
        assert_eq!(
            reg.get(ServiceEndpointKey::new(
                0x1234,
                crate::NetEndpoint::tcp(addr)
            ))
            .unwrap()
            .instance_id,
            2
        );
    }

    #[test]
    fn reinsert_same_key_replaces_in_place() {
        let mut reg = ServiceRegistry::default();
        reg.insert(key(0x47, A, 30000), info(54)).unwrap();
        reg.insert(key(0x47, A, 30000), info(55)).unwrap();
        assert_eq!(reg.get(key(0x47, A, 30000)).unwrap().instance_id, 55);
    }

    #[test]
    fn remove_one_device_leaves_the_other() {
        // Regression for "StopOffer from one device evicted all".
        let mut reg = ServiceRegistry::default();
        reg.insert(key(0x47, A, 30001), info(54)).unwrap();
        reg.insert(key(0x47, B, 30002), info(54)).unwrap();
        assert!(reg.remove(key(0x47, A, 30001)).is_some());
        assert!(reg.get(key(0x47, A, 30001)).is_none());
        assert!(reg.get(key(0x47, B, 30002)).is_some());
    }

    #[test]
    fn get_missing_returns_none() {
        let reg = ServiceRegistry::default();
        assert!(reg.get(key(0xFFFF, A, 0)).is_none());
    }

    #[test]
    fn remove_missing_returns_none() {
        let mut reg = ServiceRegistry::default();
        assert!(reg.remove(key(0xFFFF, A, 0)).is_none());
    }

    #[test]
    fn insert_returns_full_at_cap() {
        let mut reg = ServiceRegistry::default();
        for i in 0..SERVICE_REGISTRY_CAP {
            #[allow(clippy::cast_possible_truncation)]
            let k = key(i as u16, A, 30000);
            assert!(reg.insert(k, info(0)).is_ok());
        }
        assert_eq!(
            reg.insert(key(0xFFFF, A, 30000), info(0)),
            Err(ServiceRegistryFull)
        );
    }

    #[test]
    fn insert_at_cap_for_existing_key_succeeds() {
        let mut reg = ServiceRegistry::default();
        for i in 0..SERVICE_REGISTRY_CAP {
            #[allow(clippy::cast_possible_truncation)]
            let k = key(i as u16, A, 30000);
            assert!(reg.insert(k, info(0)).is_ok());
        }
        assert!(reg.insert(key(0, A, 30000), info(9999)).is_ok());
        assert_eq!(reg.get(key(0, A, 30000)).unwrap().instance_id, 9999);
    }
}
