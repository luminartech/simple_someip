use std::{collections::HashMap, net::Ipv4Addr};

use chrono::{DateTime, Utc};

use crate::{
    Error,
    protocol::sd::{self, Entry, Options, TransportProtocol},
};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DiscoveredIpV4Endpoint {
    service_id: u16,
    instance_id: u16,
    ip: Ipv4Addr,
    protocol: TransportProtocol,
    port: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndpointInfo {
    last_seen: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct DiscoveryInfo(HashMap<DiscoveredIpV4Endpoint, EndpointInfo>);

impl DiscoveryInfo {
    pub fn new() -> Self {
        Self(HashMap::new())
    }

    pub fn update(&mut self, sd_header: sd::Header) -> Result<Self, Error> {
        for entry in &sd_header.entries {
            // Just try to parse the Offer Service entry for now
            if let Entry::OfferService(service_entry) = &entry {
                let service_id = service_entry.service_id;
                let instance_id = service_entry.instance_id;
                if entry.total_options_count() == 0 {
                    return Err(Error::InvalidSDHeader(sd_header));
                }
                let endpoint_index = service_entry.index_first_options_run as usize;
                if endpoint_index >= sd_header.options.len() {
                    return Err(Error::InvalidSDHeader(sd_header));
                }
                let endpoint_option = &sd_header.options[endpoint_index];
                if let Options::IpV4Endpoint { ip, protocol, port } = endpoint_option {
                    let ip = Ipv4Addr::from(*ip);
                    let discovered = DiscoveredIpV4Endpoint {
                        service_id,
                        instance_id,
                        ip,
                        protocol: *protocol,
                        port: *port,
                    };
                    self.0.insert(
                        discovered,
                        EndpointInfo {
                            last_seen: Utc::now(),
                        },
                    );
                } else {
                    return Err(Error::InvalidSDHeader(sd_header));
                }
            }
        }
        Ok(self.clone())
    }
}
impl std::fmt::Display for DiscoveryInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Discovered SOME/IP Endpoints:\n[")?;
        for endpoint in &self.0 {
            writeln!(
                f,
                "    Service ID: {}, Instance ID: {}, IP: {}, Transport: {:?}, Port: {} - Last Seen: {}",
                endpoint.0.service_id,
                endpoint.0.instance_id,
                endpoint.0.ip,
                endpoint.0.protocol,
                endpoint.0.port,
                endpoint.1.last_seen
            )?;
        }
        writeln!(f, "]")
    }
}
