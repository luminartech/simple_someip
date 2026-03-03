use std::net::Ipv4Addr;

use simple_someip::{protocol::Error, traits::DiscoveryOnlyPayload};
use tracing::{error, info, level_filters::LevelFilter};

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        .with_max_level(LevelFilter::INFO)
        .init();
    // Bind with an interface that *doesn't* work
    let mut client =
        simple_someip::Client::<DiscoveryOnlyPayload>::new(Ipv4Addr::new(192, 168, 10, 90));
    client.bind_discovery().await.unwrap();

    // Change the interface to one that *does* work
    client
        .set_interface(Ipv4Addr::new(192, 168, 11, 87))
        .await
        .unwrap();

    while let Some(update) = client.run().await {
        match update {
            simple_someip::ClientUpdate::DiscoveryUpdated(header) => {
                info!("{header:?}")
            }
            simple_someip::ClientUpdate::Unicast(_) => todo!(),
            simple_someip::ClientUpdate::Error(error) => {
                error!("Error: {:?}", error);
            }
        }
    }
    Ok(())
}
