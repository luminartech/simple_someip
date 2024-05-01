use std::net::Ipv4Addr;

use simple_someip::{protocol::Error, ClientConfig};

fn main() -> Result<(), Error> {
    let config = ClientConfig {
        client_ip: Ipv4Addr::new(192, 168, 10, 87),
    };
    let mut client = simple_someip::SomeIPClient::new(config);
    client.connect()?;
    Ok(())
}
