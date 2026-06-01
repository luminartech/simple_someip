//! Desktop peer for testing halo's phase21 SOME/IP integration.
//!
//! Run this on a machine that's on the same Ethernet segment as the
//! sensor (e.g. 192.168.10.149 with the sensor at 192.168.10.151). It
//! mirrors the iris catalog so every halo MVP behavior is exercised:
//!
//!   • Subscribes to SVC_OBJECT (0x0001), SVC_SYSTEM (0x0002), and
//!     SVC_SCAN (0x0003) on instance 0x0097. Halo's outbound
//!     notifications land here and get logged as `[RX] <svc>/<method>
//!     len=<N>`.
//!   • Offers SVC_MODE_CTRL2 (0x005B / inst 0x0002 / eg 1) with E2E
//!     Profile 5 with-header (data_id=40046, data_length=1, max_delta=2).
//!     A periodic publisher walks the mode counter 1..6..1..6...; halo's
//!     `drain_client_rx` runs the E2E check and logs
//!     `[SOMEIP] RX SystemModeCtrl2: mode=N e2e=PASSED(1)`.
//!   • Sends periodic HWP1SystemMode method requests (svc 0x0002,
//!     method 0x8002) to the sensor's unicast port 10000. Halo's
//!     `drain_unicast_rx` parses the SOME/IP header and dispatches
//!     `s_on_event` → `[SOMEIP] RX HWP1SystemMode: req=N`.
//!
//! Usage:
//! ```text
//! cargo run -p someip_peer -- 192.168.10.149             # sensor defaults to 192.168.10.151
//! cargo run -p someip_peer -- 192.168.10.149 192.168.10.151
//! ```

use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::Duration;

use simple_someip::protocol::sd::{
    Entry, Flags, OptionsCount, Options, RebootFlag, ServiceEntry, TransportProtocol,
};
use simple_someip::protocol::{Header, Message, MessageId};
use simple_someip::server::{Server, ServerConfig};
use simple_someip::{
    ClientUpdate, E2EKey, E2EProfile, PayloadWireFormat, RawPayload, VecSdHeader,
};
use tracing::{error, info, warn};

type Payload = RawPayload;

// ── Sensor's offered catalog (we're the consumer for these) ────────────
const SVC_OBJECT: u16 = 0x0001;
const SVC_SYSTEM: u16 = 0x0002;
const SVC_SCAN: u16 = 0x0003;
const SENSOR_INSTANCE: u16 = 0x0097;
const SENSOR_UNICAST_PORT: u16 = 10000;
const EG_OBJECT: u16 = 1;
const EG_SYSTEM: u16 = 2;
const EG_SCAN: u16 = 3;

// ── Our offered catalog (sensor is the consumer here) ──────────────────
const SVC_MODE_CTRL2: u16 = 0x005B;
const MODE_CTRL2_INSTANCE: u16 = 0x0002;
const MODE_CTRL2_EG: u16 = 1;
const MODE_CTRL2_METHOD: u16 = 0x8005;
const MODE_CTRL2_E2E_DATA_ID: u16 = 40046;
const MY_SERVER_PORT: u16 = 30685;
/// Port the Client binds for receiving the sensor's iris notifications.
/// Must be distinct from `MY_SERVER_PORT` (that one is owned by the
/// Server for MODE_CTRL2). We advertise it in our Subscribe-SD's
/// IPv4Endpoint option, and tell the Client to bind there via
/// `add_endpoint(svc, inst, sensor_ep, PEER_CLIENT_RX_PORT)`.
const PEER_CLIENT_RX_PORT: u16 = 40001;

// ── Inbound HWP1 method we shoot at the sensor ─────────────────────────
const HWP1_SYSTEM_MODE_METHOD: u16 = 0x8002;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_max_level(tracing::level_filters::LevelFilter::INFO)
        .init();

    let mut args = std::env::args().skip(1);
    let interface: Ipv4Addr = args
        .next()
        .unwrap_or_else(|| {
            eprintln!("Usage: someip_peer <host-ip> [sensor-ip]");
            eprintln!("Example: someip_peer 192.168.10.149 192.168.10.151");
            std::process::exit(1);
        })
        .parse()
        .map_err(|e| format!("invalid host IP: {e}"))?;
    let sensor: Ipv4Addr = args
        .next()
        .unwrap_or_else(|| "192.168.10.151".to_string())
        .parse()
        .map_err(|e| format!("invalid sensor IP: {e}"))?;

    info!("someip_peer starting — host={interface} sensor={sensor}");

    // ── Client (subscribes to sensor's services + sends HWP1 requests) ─
    let (client, mut updates, run_fut) =
        simple_someip::Client::<Payload, _, _, _>::new(interface);
    let _client_run = tokio::spawn(run_fut);
    client.bind_discovery().await?;
    info!("client SD discovery bound on multicast 239.255.0.255:30490");

    // Tell the client the sensor's services live at sensor:10000.
    // Bypasses the FindService/OfferService dance — Subscribes go
    // straight there.
    let sensor_ep = SocketAddrV4::new(sensor, SENSOR_UNICAST_PORT);
    for (svc, name) in [
        (SVC_OBJECT, "SVC_OBJECT"),
        (SVC_SYSTEM, "SVC_SYSTEM"),
        (SVC_SCAN, "SVC_SCAN"),
    ] {
        if let Err(e) = client
            .add_endpoint(svc, SENSOR_INSTANCE, sensor_ep, PEER_CLIENT_RX_PORT)
            .await
        {
            warn!("add_endpoint(0x{svc:04X} {name}) failed: {e:?}");
        }
    }

    // ── Server (offers MODE_CTRL2 with E2E Profile 5 with-header) ──────
    let config = ServerConfig::new(SVC_MODE_CTRL2, MODE_CTRL2_INSTANCE)
        .with_interface(interface)
        .with_local_port(MY_SERVER_PORT)
        .with_major_version(1)
        .with_ttl(Duration::from_secs(3))
        .with_event_group(MODE_CTRL2_EG);
    let (server, server_handles, server_run) = Server::new(config).await?;
    info!("server bound on {interface}:{MY_SERVER_PORT} (MODE_CTRL2)");

    // Register E2E P5+header for the (0x005B, 0x8005) message-id so the
    // EventPublisher applies P5 protection on outbound notifications.
    let e2e_key = E2EKey::from_message_id(MessageId::from(
        ((SVC_MODE_CTRL2 as u32) << 16) | MODE_CTRL2_METHOD as u32,
    ));
    server.register_e2e(
        e2e_key,
        E2EProfile::Profile5WithHeader(
            simple_someip::e2e::Profile5Config::new(MODE_CTRL2_E2E_DATA_ID, 1, 2),
        ),
    )?;
    info!("E2E P5+header registered for MODE_CTRL2 (data_id={MODE_CTRL2_E2E_DATA_ID})");

    let _server_run = tokio::spawn(async move {
        if let Err(e) = server_run.await {
            error!("server.run exited: {e:?}");
        }
    });

    // ── Combined SD: Find sensor's 3 services + Offer ours ─────────────
    //
    // The sensor sees our OfferService for MODE_CTRL2 and subscribes;
    // the three FindService entries elicit OfferService responses from
    // the sensor (which the client's inner loop registers).
    let sd_header = build_sd_header(interface);
    let _announce = tokio::spawn(
        client.sd_announcements_loop(sd_header, Duration::from_secs(1)),
    );
    info!("combined Find+Offer SD announcements running (1 s cadence)");

    // ── Subscribe to each sensor service (back-to-back) ────────────────
    //
    // Halo now buffers SD bursts via an 8-deep per-port mailbox queue,
    // so we can fire Subscribes rapid-fire without the stagger that
    // earlier builds needed. Each `subscribe_no_wait` triggers
    // `bind_unicast(client_port)` inside the Client's inner loop so
    // port 40001 has a listener for the inbound notifications.
    for (svc, eg, name) in [
        (SVC_OBJECT, EG_OBJECT, "SVC_OBJECT"),
        (SVC_SYSTEM, EG_SYSTEM, "SVC_SYSTEM"),
        (SVC_SCAN, EG_SCAN, "SVC_SCAN"),
    ] {
        client
            .subscribe_no_wait(svc, SENSOR_INSTANCE, 1, 0xFFFF_FF, eg, PEER_CLIENT_RX_PORT)
            .await;
        info!("subscribe_no_wait → 0x{svc:04X} eg {eg} ({name})");
    }

    // ── Main `select!` loop: drain updates + tick publisher + tick HWP1
    //    + Ctrl-C, all in one task to avoid the `!Send` GAT futures.
    info!("running — Ctrl-C to stop");
    let publisher = server_handles.publisher.clone();
    let mut publish_ticker = tokio::time::interval(Duration::from_secs(1));
    publish_ticker.tick().await; // skip the immediate first tick
    let mut request_ticker = tokio::time::interval(Duration::from_secs(3));
    request_ticker.tick().await;
    let mut mode: u8 = 1;
    let mut req: u8 = 1;
    loop {
        tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl-C received, shutting down");
                break;
            }
            _ = publish_ticker.tick() => {
                let payload_bytes = [mode];
                let raw = match RawPayload::from_payload_bytes(
                    MessageId::new_from_service_and_method(SVC_MODE_CTRL2, MODE_CTRL2_METHOD),
                    &payload_bytes,
                ) {
                    Ok(p) => p,
                    Err(e) => { warn!("MODE_CTRL2 payload encode failed: {e:?}"); continue; }
                };
                let header = Header::new_event(
                    SVC_MODE_CTRL2,
                    MODE_CTRL2_METHOD,
                    0, // request_id
                    0x01, 0x01,
                    payload_bytes.len(),
                );
                let msg = Message::new(header, raw);
                match publisher
                    .publish_event(SVC_MODE_CTRL2, MODE_CTRL2_INSTANCE, MODE_CTRL2_EG, &msg)
                    .await
                {
                    Ok(n) => info!("[TX] MODE_CTRL2 mode={mode} → {n} subscriber(s)"),
                    Err(e) => warn!("publish_event failed: {e:?}"),
                }
                mode = if mode >= 6 { 1 } else { mode + 1 };
            }
            _ = request_ticker.tick() => {
                let payload_bytes = [req];
                let raw = match RawPayload::from_payload_bytes(
                    MessageId::new_from_service_and_method(SVC_SYSTEM, HWP1_SYSTEM_MODE_METHOD),
                    &payload_bytes,
                ) {
                    Ok(p) => p,
                    Err(e) => { warn!("HWP1 payload encode failed: {e:?}"); continue; }
                };
                let header = Header::new(
                    MessageId::new_from_service_and_method(SVC_SYSTEM, HWP1_SYSTEM_MODE_METHOD),
                    0,
                    0x01, 0x01,
                    simple_someip::protocol::MessageTypeField::new(
                        simple_someip::protocol::MessageType::Request,
                        false,
                    ),
                    simple_someip::protocol::ReturnCode::Ok,
                    payload_bytes.len(),
                );
                let msg = Message::new(header, raw);
                match client.send_to_service(SVC_SYSTEM, SENSOR_INSTANCE, msg).await {
                    Ok(_pending) => info!("[TX] HWP1SystemMode req={req} → sensor"),
                    Err(e) => warn!("send_to_service failed: {e:?}"),
                }
                req = if req >= 6 { 1 } else { req + 1 };
            }
            Some(update) = updates.recv() => {
                match update {
                    ClientUpdate::Unicast { message, e2e_status } => {
                        let svc = message.header().message_id().service_id();
                        let method = message.header().message_id().method_id();
                        let len = message.header().payload_size();
                        match e2e_status {
                            Some(s) => info!("[RX] 0x{svc:04X}/0x{method:04X} len={len} e2e={s:?}"),
                            None    => info!("[RX] 0x{svc:04X}/0x{method:04X} len={len}"),
                        }
                    }
                    ClientUpdate::DiscoveryUpdated(msg) => {
                        for entry in &msg.sd_header.entries {
                            if let Entry::OfferService(svc) = entry {
                                info!(
                                    "[SD] discovered 0x{:04X}.0x{:04X} from {}",
                                    svc.service_id, svc.instance_id, msg.source
                                );
                            }
                        }
                    }
                    ClientUpdate::SenderRebooted(addr) => warn!("[SD] sender rebooted: {addr}"),
                    ClientUpdate::Error(e) => error!("[client] error: {e:?}"),
                }
            }
        }
    }
    Ok(())
}

/// Combined SD payload: FindService × 3 (sensor's services) +
/// OfferService for our MODE_CTRL2 + IPv4 endpoint pointing at us.
/// Subscribes are NOT bundled here — they go out as separate SD
/// datagrams via `client.subscribe_no_wait` (see the staggered loop
/// in `main`). Halo's per-port mailbox slot can only hold one SD
/// datagram between polls, and a multi-entry batch seemed to register
/// only one subscription on the embedded side; staggered one-Subscribe-
/// per-SD-datagram is more reliable.
fn build_sd_header(interface: Ipv4Addr) -> VecSdHeader {
    let find = |svc| {
        Entry::FindService(ServiceEntry {
            index_first_options_run: 0,
            index_second_options_run: 0,
            options_count: OptionsCount::new(0, 0),
            service_id: svc,
            instance_id: SENSOR_INSTANCE,
            major_version: 1,
            ttl: 3,
            minor_version: 0,
        })
    };
    let offer_mode_ctrl2 = Entry::OfferService(ServiceEntry {
        index_first_options_run: 0,
        index_second_options_run: 0,
        options_count: OptionsCount::new(1, 0),
        service_id: SVC_MODE_CTRL2,
        instance_id: MODE_CTRL2_INSTANCE,
        major_version: 1,
        ttl: 3,
        minor_version: 0,
    });
    let server_endpoint = Options::IpV4Endpoint {
        ip: interface,
        protocol: TransportProtocol::Udp,
        port: MY_SERVER_PORT,
    };
    VecSdHeader {
        flags: Flags::new_sd(RebootFlag::RecentlyRebooted),
        entries: vec![
            find(SVC_OBJECT),
            find(SVC_SYSTEM),
            find(SVC_SCAN),
            offer_mode_ctrl2,
        ],
        options: vec![server_endpoint],
    }
}
