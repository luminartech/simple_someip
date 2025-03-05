use std::{net::Ipv4Addr, thread};

use simple_someip::{
    protocol::{
        Error,
        sd::{Entry, Options, TransportProtocol},
    },
    traits::DiscoveryOnlyPayload,
};

#[derive(Clone, Debug, PartialEq, Eq)]
struct DiscoveredIpV4Endpoint {
    service_id: u16,
    instance_id: u16,
    ip: Ipv4Addr,
    protocol: TransportProtocol,
    port: u16,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let mut client =
        simple_someip::Client::<DiscoveryOnlyPayload>::new(Ipv4Addr::new(192, 168, 10, 87));
    client.bind_discovery().await.unwrap();
    let mut discovered_endpoints = Vec::new();
    loop {
        let update = client.run().await;
        match update {
            simple_someip::ClientUpdate::DiscoveryUpdated(header) => {
                for entry in header.entries {
                    if let Entry::OfferService(service_entry) = &entry {
                        let service_id = service_entry.service_id;
                        let instance_id = service_entry.instance_id;
                        if entry.total_options_count() == 0 {
                            continue;
                        }
                        let endpoint_index = service_entry.index_first_options_run as usize;
                        if endpoint_index >= header.options.len() {
                            continue;
                        }
                        let endpoint_option = &header.options[endpoint_index];
                        if let Options::IpV4Endpoint { ip, protocol, port } = endpoint_option {
                            let ip = Ipv4Addr::from(*ip);
                            let discovered = DiscoveredIpV4Endpoint {
                                service_id,
                                instance_id,
                                ip,
                                protocol: *protocol,
                                port: *port,
                            };
                            if discovered_endpoints.contains(&discovered) {
                                continue;
                            } else {
                                discovered_endpoints.push(discovered);
                                print!("{}[2J", 27 as char);
                                print!("{esc}[2J{esc}[1;1H", esc = 27 as char);
                                println!("Discovered SOME/IP Endpoints:\n[");
                                for endpoint in &discovered_endpoints {
                                    println!(
                                        "    Service ID: {}, Instance ID: {}, IP: {}, Transport: {:?}, Port: {},",
                                        endpoint.service_id,
                                        endpoint.instance_id,
                                        endpoint.ip,
                                        endpoint.protocol,
                                        endpoint.port
                                    );
                                }
                                println!("]");
                            }
                        }
                    }
                }
            }
            simple_someip::ClientUpdate::Unicast(_) => todo!(),
            simple_someip::ClientUpdate::Error(error) => {
                print!("{}[2J", 27 as char);
                print!("{esc}[2J{esc}[1;1H", esc = 27 as char);
                println!("Error: {:?}", error);
            }
        }
    }
}
