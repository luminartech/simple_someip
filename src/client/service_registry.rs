use std::{collections::HashMap, net::SocketAddrV4};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ServiceInstanceId {
    pub service_id: u16,
    pub instance_id: u16,
}

#[derive(Clone, Debug)]
pub struct ServiceEndpointInfo {
    pub addr: SocketAddrV4,
    #[allow(dead_code)]
    pub major_version: u8,
    #[allow(dead_code)]
    pub minor_version: u32,
}

#[derive(Debug, Default)]
pub struct ServiceRegistry {
    endpoints: HashMap<ServiceInstanceId, ServiceEndpointInfo>,
}

impl ServiceRegistry {
    pub fn insert(&mut self, id: ServiceInstanceId, info: ServiceEndpointInfo) {
        self.endpoints.insert(id, info);
    }

    pub fn remove(&mut self, id: ServiceInstanceId) -> Option<ServiceEndpointInfo> {
        self.endpoints.remove(&id)
    }

    pub fn get(&self, id: ServiceInstanceId) -> Option<&ServiceEndpointInfo> {
        self.endpoints.get(&id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn test_id(service: u16, instance: u16) -> ServiceInstanceId {
        ServiceInstanceId {
            service_id: service,
            instance_id: instance,
        }
    }

    fn test_info(port: u16) -> ServiceEndpointInfo {
        ServiceEndpointInfo {
            addr: SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 1), port),
            major_version: 1,
            minor_version: 0,
        }
    }

    #[test]
    fn insert_and_get() {
        let mut reg = ServiceRegistry::default();
        let id = test_id(0x1234, 0x0001);
        reg.insert(id, test_info(30000));
        let info = reg.get(id).unwrap();
        assert_eq!(info.addr.port(), 30000);
        assert_eq!(info.major_version, 1);
    }

    #[test]
    fn remove_returns_info() {
        let mut reg = ServiceRegistry::default();
        let id = test_id(0x1234, 0x0001);
        reg.insert(id, test_info(30000));
        let removed = reg.remove(id).unwrap();
        assert_eq!(removed.addr.port(), 30000);
        assert!(reg.get(id).is_none());
    }

    #[test]
    fn overwrite_replaces_info() {
        let mut reg = ServiceRegistry::default();
        let id = test_id(0x1234, 0x0001);
        reg.insert(id, test_info(30000));
        reg.insert(id, test_info(40000));
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
}
