use std::net::Ipv4Addr;

use simple_someip::{
    protocol::{
        Error,
        sd::{Entry, Options, TransportProtocol},
    },
    traits::DiscoveryOnlyPayload,
};
use tracing::level_filters::LevelFilter;

#[derive(Clone, Debug, PartialEq, Eq)]
struct DiscoveredIpV4Endpoint {
    service_id: u16,
    instance_id: u16,
    ip: Ipv4Addr,
    protocol: TransportProtocol,
    port: u16,
}

fn clear_console() {
    print!("{}[2J", 27 as char);
    print!("{esc}[2J{esc}[1;1H", esc = 27 as char);
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        .with_max_level(LevelFilter::TRACE)
        .init();
    // Bind with an interface that *doesn't* work
    let mut client =
        simple_someip::Client::<DiscoveryOnlyPayload>::new(Ipv4Addr::new(192, 168, 10, 90));
    client.bind_discovery().await.unwrap();

    // Change the interface to one that *does* work
    client
        .set_interface(Ipv4Addr::new(192, 168, 10, 87))
        .await
        .unwrap();

    loop {
        let update = client.run().await;
        clear_console();
        match update {
            simple_someip::ClientUpdate::DiscoveryUpdated(header) => {
                println!("{header}")
            }
            simple_someip::ClientUpdate::Unicast(_) => todo!(),
            simple_someip::ClientUpdate::Error(error) => {
                println!("Error: {:?}", error);
            }
        }
    }
}
