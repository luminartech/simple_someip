use crate::NetEndpoint;
use core::net::IpAddr;
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
/// (`[PRS_SOMEIP_00162]`) and lives in `ServiceEndpointInfo` instead.
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

#[derive(Clone, Copy, Debug)]
pub struct ServiceEndpointInfo {
    /// SOME/IP instance id offered at this endpoint. Not part of the
    /// key (`[PRS_SOMEIP_00162]`: instance ids discriminate instances
    /// of the same service only), but `SubscribeEventgroup` SD entries
    /// carry it on the wire, so it is stored as data.
    pub instance_id: u16,
    pub local_port: u16,
    pub major_version: u8,
    #[allow(dead_code)]
    pub minor_version: u32,
    /// The address of the SD sender whose Offer registered this entry, or
    /// `None` for an entry only `Client::add_endpoint` registered.
    pub offered_by: Option<IpAddr>,
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

    /// Registers `key` for an Offer described by `offer`. An entry already at
    /// `key` keeps its `local_port`, so that a re-offer does not undo
    /// `Client::add_endpoint`.
    pub fn record_offer(
        &mut self,
        key: ServiceEndpointKey,
        offer: ServiceEndpointInfo,
    ) -> Result<(), ServiceRegistryFull> {
        let local_port = self
            .endpoints
            .get(&key)
            .map_or(offer.local_port, |e| e.local_port);
        self.insert(
            key,
            ServiceEndpointInfo {
                local_port,
                ..offer
            },
        )
    }

    /// Registers `key` for `Client::add_endpoint`. An entry an Offer already
    /// registered at `key` keeps that Offer's sender and versions, so that its
    /// stop offer still removes it.
    pub fn add_endpoint(
        &mut self,
        key: ServiceEndpointKey,
        instance_id: u16,
        local_port: u16,
    ) -> Result<(), ServiceRegistryFull> {
        let offered = self.endpoints.get(&key);
        let info = ServiceEndpointInfo {
            instance_id,
            local_port,
            major_version: offered.map_or(0xFF, |offered| offered.major_version),
            minor_version: offered.map_or(0xFFFF_FFFF, |offered| offered.minor_version),
            offered_by: offered.and_then(|offered| offered.offered_by),
        };
        self.insert(key, info)
    }

    pub fn remove(&mut self, key: ServiceEndpointKey) -> Option<ServiceEndpointInfo> {
        self.endpoints.swap_remove(&key)
    }

    /// Removes every entry of `service_id` instance `instance_id` at
    /// `major_version` that `sender` offered, returning how many were removed.
    pub fn remove_offered_instance(
        &mut self,
        service_id: u16,
        instance_id: u16,
        major_version: u8,
        sender: IpAddr,
    ) -> usize {
        let before = self.endpoints.len();
        self.endpoints.retain(|key, info| {
            !(key.service_id == service_id
                && info.instance_id == instance_id
                && info.major_version == major_version
                && info.offered_by == Some(sender))
        });
        before - self.endpoints.len()
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
            offered_by: None,
        }
    }
    fn offered_info(instance: u16, major_version: u8, sender: Ipv4Addr) -> ServiceEndpointInfo {
        ServiceEndpointInfo {
            major_version,
            offered_by: Some(sender.into()),
            ..info(instance)
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
    fn add_endpoint_keeps_the_offer_that_registered_the_endpoint() {
        let mut reg = ServiceRegistry::default();
        let offered = ServiceEndpointInfo {
            offered_by: Some(A.into()),
            ..info(54)
        };
        reg.insert(key(0x47, A, 30001), offered).unwrap();
        reg.add_endpoint(key(0x47, A, 30001), 54, 40001).unwrap();
        reg.add_endpoint(key(0x47, A, 30002), 54, 40002).unwrap();
        assert_eq!(reg.get(key(0x47, A, 30001)).unwrap().local_port, 40001);
        assert_eq!(reg.remove_offered_instance(0x47, 54, 1, A.into()), 1);
        assert!(reg.get(key(0x47, A, 30001)).is_none());
        assert!(reg.get(key(0x47, A, 30002)).is_some());
    }

    #[test]
    fn an_offer_keeps_the_local_port_add_endpoint_set() {
        let mut reg = ServiceRegistry::default();
        reg.add_endpoint(key(0x47, A, 30001), 54, 40001).unwrap();
        reg.record_offer(key(0x47, A, 30001), offered_info(54, 1, A))
            .unwrap();
        let entry = reg.get(key(0x47, A, 30001)).unwrap();
        assert_eq!(entry.local_port, 40001);
        assert_eq!(entry.offered_by, Some(A.into()));
    }

    #[test]
    fn remove_offered_instance_removes_only_what_that_sender_offered() {
        let offered = offered_info;
        let mut reg = ServiceRegistry::default();
        reg.insert(key(0x47, B, 30001), offered(54, 1, A)).unwrap();
        reg.insert(key(0x47, B, 30002), offered(55, 1, A)).unwrap();
        reg.insert(key(0x47, B, 30003), offered(54, 2, A)).unwrap();
        reg.insert(key(0x47, B, 30004), offered(54, 1, B)).unwrap();
        reg.insert(key(0x47, B, 30005), info(54)).unwrap();
        assert_eq!(reg.remove_offered_instance(0x47, 54, 1, A.into()), 1);
        assert!(reg.get(key(0x47, B, 30001)).is_none());
        for port in 30002..=30005 {
            assert!(reg.get(key(0x47, B, port)).is_some(), "port {port}");
        }
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
