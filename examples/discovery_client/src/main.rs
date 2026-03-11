use std::{collections::HashMap, fmt, net::Ipv4Addr};

use simple_someip::{
    RawPayload,
    protocol::{
        Error,
        sd::{Entry, Options, TransportProtocol},
    },
};
use tracing::{error, info, level_filters::LevelFilter, warn};

type Payload = RawPayload;

/// Endpoint information extracted from SD options.
#[derive(Clone)]
struct Endpoint {
    ip: IpAddr,
    port: u16,
    protocol: TransportProtocol,
}

/// Unified IP address for display purposes.
#[derive(Clone)]
enum IpAddr {
    V4(Ipv4Addr),
    V6(std::net::Ipv6Addr),
}

impl fmt::Display for IpAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IpAddr::V4(ip) => write!(f, "{ip}"),
            IpAddr::V6(ip) => write!(f, "{ip}"),
        }
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let proto = match self.protocol {
            TransportProtocol::Udp => "UDP",
            TransportProtocol::Tcp => "TCP",
        };
        write!(f, "{}:{} ({proto})", self.ip, self.port)
    }
}

/// Tracked state for a discovered service.
struct ServiceInfo {
    major_version: u8,
    minor_version: u32,
    ttl: u32,
    endpoints: Vec<Endpoint>,
    offer_count: u64,
}

impl fmt::Display for ServiceInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "v{}.{} TTL={} offers={} endpoints=[{}]",
            self.major_version,
            self.minor_version,
            self.ttl,
            self.offer_count,
            self.endpoints
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", "),
        )
    }
}

/// Tracked state for an event group.
struct EventGroupInfo {
    major_version: u8,
    ttl: u32,
    counter: u16,
    subscribe_count: u64,
    ack_count: u64,
    nack_count: u64,
}

impl fmt::Display for EventGroupInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "v{} TTL={} counter={} subscribes={} acks={} nacks={}",
            self.major_version,
            self.ttl,
            self.counter,
            self.subscribe_count,
            self.ack_count,
            self.nack_count,
        )
    }
}

/// Key for identifying a service: (`service_id`, `instance_id`).
type ServiceKey = (u16, u16);

/// Key for identifying an event group: (`service_id`, `instance_id`, `event_group_id`).
type EventGroupKey = (u16, u16, u16);

struct DiscoveryState {
    services: HashMap<ServiceKey, ServiceInfo>,
    event_groups: HashMap<EventGroupKey, EventGroupInfo>,
    total_messages: u64,
    find_service_count: u64,
}

impl DiscoveryState {
    fn new() -> Self {
        Self {
            services: HashMap::new(),
            event_groups: HashMap::new(),
            total_messages: 0,
            find_service_count: 0,
        }
    }

    fn process_entry(&mut self, entry: &Entry, options: &[Options]) {
        match entry {
            Entry::OfferService(svc) => {
                let key = (svc.service_id, svc.instance_id);
                let endpoints = extract_endpoints(
                    svc.index_first_options_run,
                    svc.options_count.first_options_count,
                    options,
                );
                let is_new = !self.services.contains_key(&key);
                let info = self.services.entry(key).or_insert(ServiceInfo {
                    major_version: svc.major_version,
                    minor_version: svc.minor_version,
                    ttl: svc.ttl,
                    endpoints: Vec::new(),
                    offer_count: 0,
                });
                info.major_version = svc.major_version;
                info.minor_version = svc.minor_version;
                info.ttl = svc.ttl;
                info.endpoints = endpoints;
                info.offer_count += 1;

                if is_new {
                    info!(
                        "NEW service 0x{:04X}.0x{:04X} v{}.{}",
                        svc.service_id, svc.instance_id, svc.major_version, svc.minor_version,
                    );
                }
            }
            Entry::StopOfferService(svc) => {
                let key = (svc.service_id, svc.instance_id);
                if self.services.remove(&key).is_some() {
                    warn!(
                        "REMOVED service 0x{:04X}.0x{:04X}",
                        svc.service_id, svc.instance_id,
                    );
                }
            }
            Entry::FindService(svc) => {
                self.find_service_count += 1;
                info!(
                    "FindService 0x{:04X}.0x{:04X}",
                    svc.service_id, svc.instance_id,
                );
            }
            Entry::SubscribeEventGroup(eg) => {
                let key = (eg.service_id, eg.instance_id, eg.event_group_id);
                let is_new = !self.event_groups.contains_key(&key);
                let info = self.event_groups.entry(key).or_insert(EventGroupInfo {
                    major_version: eg.major_version,
                    ttl: eg.ttl,
                    counter: eg.counter,
                    subscribe_count: 0,
                    ack_count: 0,
                    nack_count: 0,
                });
                info.major_version = eg.major_version;
                info.ttl = eg.ttl;
                info.counter = eg.counter;
                info.subscribe_count += 1;

                if is_new {
                    info!(
                        "NEW subscription 0x{:04X}.0x{:04X} group=0x{:04X}",
                        eg.service_id, eg.instance_id, eg.event_group_id,
                    );
                }
            }
            Entry::SubscribeAckEventGroup(eg) => {
                let key = (eg.service_id, eg.instance_id, eg.event_group_id);
                let info = self.event_groups.entry(key).or_insert(EventGroupInfo {
                    major_version: eg.major_version,
                    ttl: eg.ttl,
                    counter: eg.counter,
                    subscribe_count: 0,
                    ack_count: 0,
                    nack_count: 0,
                });
                info.major_version = eg.major_version;
                info.counter = eg.counter;

                if eg.ttl == 0 {
                    info.nack_count += 1;
                    warn!(
                        "Subscribe NACK 0x{:04X}.0x{:04X} group=0x{:04X}",
                        eg.service_id, eg.instance_id, eg.event_group_id,
                    );
                } else {
                    info.ttl = eg.ttl;
                    info.ack_count += 1;
                    info!(
                        "Subscribe ACK 0x{:04X}.0x{:04X} group=0x{:04X}",
                        eg.service_id, eg.instance_id, eg.event_group_id,
                    );
                }
            }
        }
    }

    fn print_summary(&self) {
        info!(
            "--- Discovery State ({} messages, {} FindService requests) ---",
            self.total_messages, self.find_service_count,
        );
        if self.services.is_empty() {
            info!("  No services discovered");
        } else {
            for (&(sid, iid), svc) in &self.services {
                info!("  Service 0x{sid:04X}.0x{iid:04X}: {svc}");
            }
        }
        if !self.event_groups.is_empty() {
            for (&(sid, iid, egid), eg) in &self.event_groups {
                info!("  EventGroup 0x{sid:04X}.0x{iid:04X}.0x{egid:04X}: {eg}");
            }
        }
        info!("---");
    }
}

/// Extract endpoints from the options array for a given entry's option run.
fn extract_endpoints(option_index: u8, option_count: u8, all_options: &[Options]) -> Vec<Endpoint> {
    let start = usize::from(option_index);
    let end = start + usize::from(option_count);
    (start..end.min(all_options.len()))
        .filter_map(|i| match &all_options[i] {
            Options::IpV4Endpoint { ip, protocol, port }
            | Options::IpV4Multicast { ip, protocol, port }
            | Options::IpV4SD { ip, protocol, port } => Some(Endpoint {
                ip: IpAddr::V4(*ip),
                port: *port,
                protocol: *protocol,
            }),
            Options::IpV6Endpoint { ip, protocol, port }
            | Options::IpV6Multicast { ip, protocol, port }
            | Options::IpV6SD { ip, protocol, port } => Some(Endpoint {
                ip: IpAddr::V6(*ip),
                port: *port,
                protocol: *protocol,
            }),
            Options::Configuration { .. } | Options::LoadBalancing { .. } => None,
        })
        .collect()
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        .with_max_level(LevelFilter::INFO)
        .init();

    let interface: Ipv4Addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| {
            eprintln!("Usage: discovery_client <interface_ip>");
            eprintln!("Example: discovery_client 192.168.1.100");
            std::process::exit(1);
        })
        .parse()
        .unwrap_or_else(|e| {
            eprintln!("Invalid IP address: {e}");
            std::process::exit(1);
        });

    info!("Starting discovery client on interface {interface}");

    let mut client = simple_someip::Client::<Payload>::new(interface);
    client.bind_discovery().await.unwrap();

    let mut state = DiscoveryState::new();

    while let Some(update) = client.run().await {
        match update {
            simple_someip::ClientUpdate::DiscoveryUpdated(msg) => {
                state.total_messages += 1;

                info!(
                    "SD from {} (session_id=0x{:04X})",
                    msg.source,
                    msg.someip_header.request_id() & 0xFFFF,
                );

                let header = &msg.sd_header;
                let options = header.options.clone();

                for entry in &header.entries {
                    state.process_entry(entry, &options);
                }

                state.print_summary();
            }
            simple_someip::ClientUpdate::SenderRebooted(addr) => {
                warn!("Sender {addr} rebooted — clearing cached state");
                state.services.clear();
                state.event_groups.clear();
            }
            simple_someip::ClientUpdate::Unicast { message, .. } => {
                info!("Unicast message: {:?}", message.header());
            }
            simple_someip::ClientUpdate::Error(err) => {
                error!("Error: {err:?}");
            }
        }
    }
    Ok(())
}
