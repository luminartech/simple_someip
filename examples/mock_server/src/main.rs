//! # Mock SOME/IP Server
//!
//! Minimal SOME/IP server that simulates a sensor offering a configurable service
//! with periodic event publishing. Useful for testing clients without real hardware.
//!
//! By default, simulates an Iris sensor's System Mode Service (0x47) with heartbeat
//! events every 300ms.
//!
//! ```sh
//! cargo run -p mock_server -- --interface 192.168.11.87
//! ```

use clap::Parser;
use simple_someip::server::ServerConfig;
use simple_someip::Server;
use std::net::Ipv4Addr;
use tokio::time::{Duration, interval};
use tracing::info;

#[derive(Parser, Debug)]
#[command(author, version, about = "Mock SOME/IP server for client testing")]
struct Args {
    /// Local interface IP address
    #[arg(short, long, default_value = "192.168.11.87")]
    interface: Ipv4Addr,

    /// Server port for receiving subscriptions
    #[arg(short, long, default_value_t = 30640)]
    port: u16,

    /// Service ID to offer
    #[arg(long, default_value_t = 0x47)]
    service_id: u16,

    /// Instance ID
    #[arg(long, default_value_t = 54)]
    instance_id: u16,

    /// Event group ID to publish on
    #[arg(long, default_value_t = 0x0003)]
    event_group: u16,

    /// Event ID (method ID) for published events
    #[arg(long, default_value_t = 0x8008)]
    event_id: u16,

    /// Event publish interval in milliseconds
    #[arg(long, default_value_t = 300)]
    interval_ms: u64,
}

/// Encode a 22-byte Iris SystemHeartbeat payload:
/// system_mode(1) + 5 bools(5) + battery_voltage(4) + system_temp(4) + reserved(8)
fn make_iris_heartbeat() -> Vec<u8> {
    let mut buf = Vec::with_capacity(22);
    buf.push(7); // SystemModeResponse::Active
    buf.push(1); // system_ok
    buf.push(1); // laser_ok
    buf.push(1); // scanner_ok
    buf.push(1); // receiver_ok
    buf.push(1); // datapath_ok
    buf.extend_from_slice(&12.5_f32.to_be_bytes()); // battery_voltage
    buf.extend_from_slice(&35.0_f32.to_be_bytes()); // system_temp
    buf.extend_from_slice(&0.0_f32.to_be_bytes()); // reserved
    buf.extend_from_slice(&0.0_f32.to_be_bytes()); // reserved2
    buf
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    info!(
        "Starting mock server on {}:{} — service 0x{:04X}, instance {}, event group 0x{:04X}, event 0x{:04X}, interval {}ms",
        args.interface, args.port, args.service_id, args.instance_id,
        args.event_group, args.event_id, args.interval_ms
    );

    let config = ServerConfig::new(
        args.interface,
        args.port,
        args.service_id,
        args.instance_id,
    );

    let mut server: Server = Server::new(config).await?;
    let publisher = server.publisher();
    server.start_announcing()?;

    info!("SD announcements started. Waiting for subscribers...");

    // Spawn server event loop (handles subscription requests)
    tokio::spawn(async move { server.run().await });

    let mut tick = interval(Duration::from_millis(args.interval_ms));
    let mut session_id: u32 = 0;
    let payload = make_iris_heartbeat();

    loop {
        tick.tick().await;

        session_id = session_id.wrapping_add(1);

        let count = publisher
            .publish_raw_event(
                args.service_id,
                args.instance_id,
                args.event_group,
                args.event_id,
                session_id,
                0x01, // protocol version
                0x01, // interface version
                &payload,
            )
            .await?;

        if count > 0 {
            info!("Event #{session_id} sent to {count} subscriber(s)");
        }
    }
}
