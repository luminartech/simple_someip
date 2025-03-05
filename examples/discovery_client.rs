use std::net::Ipv4Addr;

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
    loop {
        let update = client.run().await;
        match update {
            simple_someip::ClientUpdate::DiscoveryUpdated(header) => {
                print!("{}[2J", 27 as char);
                print!("{esc}[2J{esc}[1;1H", esc = 27 as char);
                println!("{header}")
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
