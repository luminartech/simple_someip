//! Client+Server hybrid example using `Client::sd_announcements_loop`.
//!
//! Demonstrates how to run a SOME/IP application that is simultaneously:
//! - A **client** subscribing to a remote service's events
//! - A **server** offering its own service for remote nodes to subscribe to
//!
//! The key pattern: when acting as both client and server, periodic SD
//! announcements must bundle `FindService` (client role) and `OfferService`
//! (server role) in the same SD message, sent from the client's SD socket.
//! This ensures remote nodes see a single coherent network identity for
//! multicast announcements.
//!
//! The server's built-in `announcement_loop()` is NOT used — instead, the
//! client's `sd_announcements_loop()` handles periodic multicast
//! announcements. The server's `run()` loop still handles unicast SD
//! traffic (e.g. `SubscribeAck`/`SubscribeNack` responses) on its own
//! socket, which is necessary for subscription management.
//!
//! Usage:
//! ```text
//! cargo run -p client_server -- <interface_ip>
//! ```
//!
//! Example:
//! ```text
//! cargo run -p client_server -- 192.168.11.87
//! ```

use std::net::Ipv4Addr;
use std::time::Duration;

use simple_someip::protocol::sd::{
    Entry, Flags, Options, OptionsCount, RebootFlag, ServiceEntry, TransportProtocol,
};
use simple_someip::server::{Server, ServerConfig};
use simple_someip::{ClientUpdate, RawPayload, VecSdHeader};
use tracing::{error, info, warn};

type Payload = RawPayload;

// Example service IDs — replace with your application's values.
const MY_SERVER_SERVICE_ID: u16 = 0x1234;
const MY_SERVER_INSTANCE_ID: u16 = 0x0001;
const MY_SERVER_PORT: u16 = 40000;

const REMOTE_SERVICE_ID: u16 = 0x5678;
const REMOTE_INSTANCE_ID: u16 = 0x0001;

/// Build the combined `FindService` + `OfferService` SD header.
fn build_sd_header(interface: Ipv4Addr) -> VecSdHeader {
    let find_remote = Entry::FindService(ServiceEntry {
        index_first_options_run: 0,
        index_second_options_run: 0,
        options_count: OptionsCount::new(0, 0),
        service_id: REMOTE_SERVICE_ID,
        instance_id: REMOTE_INSTANCE_ID,
        major_version: 1,
        ttl: 3,
        minor_version: 0,
    });

    let offer_mine = Entry::OfferService(ServiceEntry {
        index_first_options_run: 0,
        index_second_options_run: 0,
        options_count: OptionsCount::new(1, 0),
        service_id: MY_SERVER_SERVICE_ID,
        instance_id: MY_SERVER_INSTANCE_ID,
        major_version: 1,
        ttl: 3,
        minor_version: 0,
    });

    let endpoint = Options::IpV4Endpoint {
        ip: interface,
        protocol: TransportProtocol::Udp,
        port: MY_SERVER_PORT,
    };

    VecSdHeader {
        flags: Flags::new_sd(RebootFlag::RecentlyRebooted),
        entries: vec![find_remote, offer_mine],
        options: vec![endpoint],
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_max_level(tracing::level_filters::LevelFilter::INFO)
        .init();

    let interface: Ipv4Addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| {
            eprintln!("Usage: client_server <interface_ip>");
            std::process::exit(1);
        })
        .parse()
        .unwrap_or_else(|e| {
            eprintln!("Invalid IP: {e}");
            std::process::exit(1);
        });

    info!("Starting client+server on {interface}");

    // ── Create the client (handles discovery, subscriptions, SD socket) ──

    let (client, mut updates, run_fut) = simple_someip::Client::<Payload>::new(interface);
    tokio::spawn(run_fut);
    client.bind_discovery().await?;
    info!("Client discovery bound");

    // ── Create the server (handles subscription requests, event publishing) ──

    let config = ServerConfig {
        interface,
        local_port: MY_SERVER_PORT,
        service_id: MY_SERVER_SERVICE_ID,
        instance_id: MY_SERVER_INSTANCE_ID,
        major_version: 1,
        minor_version: 0,
        ttl: 3,
    };

    let mut server = Server::new(config).await?;
    info!("Server bound on port {MY_SERVER_PORT}");

    // NOTE: We intentionally do NOT spawn server.announcement_loop().
    // The client's sd_announcements_loop handles all SD traffic.

    let _publisher = server.publisher();

    // Spawn the server event loop (handles incoming subscriptions).
    let _server_handle = tokio::spawn(async move {
        if let Err(e) = server.run().await {
            error!("Server error: {e}");
        }
    });

    // ── Start combined SD announcements from the client socket ───────────

    let sd_header = build_sd_header(interface);
    let _announce_handle =
        tokio::spawn(client.sd_announcements_loop(sd_header, Duration::from_secs(1)));
    info!("Started combined Find+Offer SD announcements (1s interval)");

    // ── Main event loop ─────────────────────────────────────────────────

    info!("Running — press Ctrl-C to stop");

    while let Some(update) = updates.recv().await {
        match update {
            ClientUpdate::DiscoveryUpdated(msg) => {
                for entry in &msg.sd_header.entries {
                    match entry {
                        Entry::OfferService(svc) => {
                            info!(
                                "Discovered service 0x{:04X}.0x{:04X} from {}",
                                svc.service_id, svc.instance_id, msg.source,
                            );
                        }
                        Entry::SubscribeAckEventGroup(eg) => {
                            info!(
                                "Subscription ACK for 0x{:04X} group 0x{:04X}",
                                eg.service_id, eg.event_group_id,
                            );
                        }
                        _ => {}
                    }
                }
            }
            ClientUpdate::SenderRebooted(addr) => {
                warn!("Sender {addr} rebooted");
            }
            ClientUpdate::Unicast { message, .. } => {
                info!(
                    "Received unicast: service=0x{:04X}",
                    message.header().message_id().service_id(),
                );
            }
            ClientUpdate::Error(e) => {
                error!("Error: {e:?}");
            }
        }
    }

    Ok(())
}
