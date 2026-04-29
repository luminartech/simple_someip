//! Phase 20f — Conformance test against the COVESA vsomeip reference
//! SOME/IP-SD implementation.
//!
//! `#[ignore]`'d by default. Run on demand once you have vsomeip
//! running on the host network (see "Running locally" below). This is
//! the first test in the simple-someip crate that catches **protocol
//! non-compliance** bugs against an external reference, vs. our
//! existing tests which all run simple-someip on both sides of the
//! wire and only catch internal-consistency issues.
//!
//! Goal of THIS test (deliberately tight scope for a first POC):
//! prove that simple-someip's `Client` can `bind_discovery()` and see
//! a vsomeip-emitted `OfferService` for a known service+instance ID
//! within a timeout. That single signal is the load-bearing wire-
//! conformance check we have zero of today.
//!
//! Subsequent phases will layer Subscribe/Ack roundtrips,
//! request/response, E2E protect/check, etc. against the same
//! vsomeip peer.
//!
//! # Running locally
//!
//! 1. Build the offerer image (one-time, ~5-10 min):
//!
//!    ```text
//!    docker build --network=host -t vsomeip-offerer \
//!        tests/data/vsomeip-offerer/
//!    ```
//!
//! 2. Find a multicast-capable interface IP on your host. **Do not
//!    use 127.0.0.1** — Linux's `lo` interface lacks the `MULTICAST`
//!    flag by default, so SD multicast (`224.0.23.0`) never leaves
//!    the host:
//!
//!    ```text
//!    ip route get 224.0.23.0
//!    # multicast 224.0.23.0 dev wlp0s20f3 src 192.168.1.42 ...
//!    #                                        ^^^^^^^^^^^^
//!    ```
//!
//!    The `src` IP is what you pass on both sides below.
//!
//! 3. Start the offerer (host-network mode so SD multicast flows on
//!    the actual interface):
//!
//!    ```text
//!    docker run --rm -d --name vsomeip-offerer --network host \
//!        -e VSOMEIP_UNICAST=192.168.1.42 \
//!        vsomeip-offerer
//!    ```
//!
//!    Verify it's emitting:
//!
//!    ```text
//!    docker logs vsomeip-offerer | grep -E "Joining|OFFER"
//!    # Joining to multicast group 224.0.23.0 from 192.168.1.42
//!    # OFFER(1277): [1234.0001:1.0] (true)
//!    ```
//!
//! 4. Run the test (use the same interface IP):
//!
//!    ```text
//!    SIMPLE_SOMEIP_TEST_INTERFACE=192.168.1.42 \
//!    cargo test --features client-tokio,server-tokio \
//!      --test vsomeip_sd_compat -- --ignored --nocapture
//!    ```
//!
//!    Expected: `client_sees_vsomeip_offer_service ... ok` in well
//!    under a second.
//!
//! 5. Tear down: `docker stop vsomeip-offerer`.
//!
//! ## Running the TX-direction tests
//!
//! There are two TX-direction tests with different tradeoffs:
//!
//! ### `tx_announcement_loop_emits_wire_format_offer` — no docker, CI-friendly
//!
//! Drives `Server::announcement_loop()` and captures the emitted bytes
//! on a second socket joined to the SD multicast group on the same
//! interface, then asserts every field of the SOME/IP + SD envelope
//! against expected values. No external reference impl involved —
//! the assertion is "the bytes match what AUTOSAR SOME/IP-SD says
//! they should be." This is the same wire format vsomeip's parser
//! consumes, so a regression here is a regression against vsomeip
//! too. Runnable in any environment whose chosen interface carries
//! the `MULTICAST` flag (loopback usually does **not** by default;
//! pass `SIMPLE_SOMEIP_TEST_INTERFACE=<iface IP>` to use a real NIC):
//!
//! ```text
//! SIMPLE_SOMEIP_TEST_INTERFACE=192.168.1.42 \
//! cargo test --features client-tokio,server-tokio \
//!     --test vsomeip_sd_compat \
//!     tx_announcement_loop_emits_wire_format_offer \
//!     -- --ignored --nocapture
//! ```
//!
//! ### `vsomeip_sees_simple_someip_offer_service` — full cross-impl
//!
//! Same image as the RX test, different role. Start a subscriber
//! container with the special name the test expects:
//!
//! ```text
//! docker run --rm -d --name vsomeip-test-subscriber --network host \
//!     -e VSOMEIP_UNICAST=192.168.1.42 \
//!     -e VSOMEIP_ROLE=subscriber \
//!     vsomeip-offerer
//! ```
//!
//! Then run the test (subscriber container runs in parallel; the
//! test starts simple-someip's `Server::announcement_loop` and polls
//! `docker logs` for the AVAILABLE marker):
//!
//! ```text
//! SIMPLE_SOMEIP_TEST_INTERFACE=192.168.1.42 \
//! cargo test --features client-tokio,server-tokio \
//!     --test vsomeip_sd_compat \
//!     vsomeip_sees_simple_someip_offer_service \
//!     -- --ignored --nocapture
//! ```
//!
//! Tear down: `docker stop vsomeip-test-subscriber`.
//!
//! **Same-host caveat (observed 2026-04-29):** running the subscriber
//! container in `--network host` mode on the same machine that's
//! running the simple-someip Server can fail to deliver multicast
//! even though `tcpdump` confirms the OfferService packets are on the
//! wire and `/proc/net/igmp` confirms the subscriber joined the
//! group. The same setup also fails vsomeip-offerer → vsomeip-
//! subscriber on the same host, so this is a vsomeip routing-host
//! quirk (both endpoints bind `0.0.0.0:30490` with `SO_REUSEPORT` and
//! one of them wins the multicast delivery non-deterministically),
//! not a simple-someip wire-format bug. Run the subscriber container
//! on a **second host** sharing the same multicast-capable network
//! to get a clean cross-impl signal. The
//! `tx_announcement_loop_emits_wire_format_offer` test above
//! sidesteps this entirely.
//!
//! # Why `#[ignore]`?
//!
//! The test depends on an external vsomeip container being up. CI
//! runners don't have that today; flipping it on `cargo test` would
//! fail 100% of CI builds. Until we have a CI step that brings up
//! vsomeip via TestContainers-rs (or equivalent), this test runs on
//! demand only.
//!
//! # Why `127.0.0.1` defaults?
//!
//! Loopback is the easiest network model for an initial POC — it
//! avoids needing a real NIC, multicast-capable bridge, or specific
//! interface IP detection. SOME/IP-SD multicast over loopback works
//! on Linux when both sides set `IP_MULTICAST_LOOP` (which our
//! `Server::new_with_loopback` does, and vsomeip's default does).
//! For real-NIC testing, set `SIMPLE_SOMEIP_TEST_INTERFACE` to the
//! interface's IP and configure vsomeip's `unicast` field to match.

#![cfg(all(feature = "client-tokio", feature = "server-tokio"))]

use std::env;
use std::net::Ipv4Addr;
use std::str::FromStr;
use std::time::Duration;

use simple_someip::protocol::sd::{self, EntryType, RebootFlag, TransportProtocol};
use simple_someip::protocol::{MessageType, MessageView, ReturnCode};
use simple_someip::server::ServerConfig;
use simple_someip::{Client, ClientUpdate, RawPayload, Server};

/// Service + instance ID the vsomeip-offerer config (above) must
/// match. Hardcoded to keep the test minimal; if you change the
/// config, change these.
const SERVICE_ID: u16 = 0x1234;
const INSTANCE_ID: u16 = 0x0001;

/// Default timeout for the SD `OfferService` to land on the
/// Client's update stream. vsomeip's default
/// `initial_delay_max = 100` ms + a few `repetitions_base_delay
/// = 200` ms ticks, so 30 s is generous.
const SD_TIMEOUT: Duration = Duration::from_secs(30);

/// Default interface if `SIMPLE_SOMEIP_TEST_INTERFACE` is unset.
/// `127.0.0.1` matches the `vsomeip-offerer.json` `"unicast"`
/// field above.
const DEFAULT_INTERFACE: Ipv4Addr = Ipv4Addr::LOCALHOST;

fn test_interface() -> Ipv4Addr {
    match env::var("SIMPLE_SOMEIP_TEST_INTERFACE") {
        Ok(s) => Ipv4Addr::from_str(s.trim())
            .unwrap_or_else(|_| panic!("SIMPLE_SOMEIP_TEST_INTERFACE not a valid IPv4: {s}")),
        Err(_) => DEFAULT_INTERFACE,
    }
}

/// Verifies simple-someip's `Client` sees vsomeip's `OfferService`
/// SD broadcast for the configured service + instance ID.
///
/// `#[ignore]` because the test depends on an external vsomeip
/// container being up — see this file's module-level docs for the
/// docker setup.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires external vsomeip-offerer container; see module docs"]
async fn client_sees_vsomeip_offer_service() {
    // Initialize tracing if RUST_LOG is set so the test prints
    // simple-someip's SD-receive logs alongside `[client] received`
    // events. Helpful when the test fails and you want to know whether
    // simple-someip got bytes at all.
    let _ = tracing_subscriber::fmt::try_init();

    let interface = test_interface();
    eprintln!("[test] listening on interface {interface}");
    eprintln!(
        "[test] expecting vsomeip OfferService(service=0x{:04X}, \
         instance=0x{:04X}) within {}s",
        SERVICE_ID,
        INSTANCE_ID,
        SD_TIMEOUT.as_secs()
    );

    // Build a tokio-flavor Client with multicast loopback enabled so
    // a vsomeip container running on the same host (host-network
    // mode) gets to send + we get to receive on the same loopback
    // interface.
    let (client, mut updates, run_fut) =
        Client::<RawPayload, _, _, _>::new_with_loopback(interface, true);

    // Spawn the run-loop. `tokio::spawn` works because the tokio
    // backend's run future is `Send + 'static`.
    let run_handle = tokio::spawn(run_fut);

    // Bind the SD multicast socket. Without this no SD traffic
    // surfaces.
    client
        .bind_discovery()
        .await
        .expect("bind_discovery failed (network setup problem?)");
    eprintln!("[test] bind_discovery OK; waiting for OfferService");

    // Drain the update stream until either (a) we see an
    // `OfferService` matching the expected service+instance, or
    // (b) the timeout fires.
    let saw_offer = tokio::time::timeout(SD_TIMEOUT, async {
        while let Some(update) = updates.recv().await {
            let ClientUpdate::DiscoveryUpdated(msg) = update else {
                eprintln!("[test] ignoring non-Discovery update: {update:?}");
                continue;
            };
            // The SD message may carry multiple entries; scan for an
            // `OfferService` matching our (service, instance).
            for entry in &msg.sd_header.entries {
                use simple_someip::protocol::sd::Entry;
                if let Entry::OfferService(svc) = entry
                    && svc.service_id == SERVICE_ID
                    && svc.instance_id == INSTANCE_ID
                {
                    eprintln!(
                        "[test] matched OfferService from {} (ttl={}, mv={}.{})",
                        msg.source, svc.ttl, svc.major_version, svc.minor_version
                    );
                    return true;
                }
            }
            eprintln!(
                "[test] saw DiscoveryUpdated from {} but no matching OfferService entry",
                msg.source
            );
        }
        false
    })
    .await;

    run_handle.abort();

    match saw_offer {
        Ok(true) => {
            eprintln!("[test] PASS — simple-someip Client matched vsomeip's OfferService SD entry");
        }
        Ok(false) => {
            panic!(
                "Update stream closed before OfferService(service=0x{SERVICE_ID:04X}, \
                 instance=0x{INSTANCE_ID:04X}) arrived. \
                 Most likely cause: vsomeip's run loop crashed or never started. \
                 Check `docker logs vsomeip-offerer`."
            )
        }
        Err(_) => {
            panic!(
                "Timed out after {}s waiting for OfferService(service=0x{SERVICE_ID:04X}, \
                 instance=0x{INSTANCE_ID:04X}). Possibilities (rough order of likelihood): \
                 (1) vsomeip container not running on host network — try `docker ps`; \
                 (2) vsomeip's `unicast` config doesn't match the listening interface — \
                 set SIMPLE_SOMEIP_TEST_INTERFACE accordingly; \
                 (3) firewall dropping multicast 224.0.23.0:30490 — try `sudo iptables -L`; \
                 (4) vsomeip configured with a different service ID — recheck the JSON; \
                 (5) genuine bug in simple-someip's SD-receive path (least likely \
                 given existing loopback tests pass).",
                SD_TIMEOUT.as_secs()
            );
        }
    }
}

// ── Phase 20h: TX direction — simple-someip emits, vsomeip subscribes ─

/// Container name for the subscriber-role container. Hardcoded so the
/// test knows which `docker logs` to scrape; if you run the container
/// under a different name, change this constant.
const SUBSCRIBER_CONTAINER: &str = "vsomeip-test-subscriber";

/// Expected log marker emitted by `subscriber.cpp`'s availability
/// handler when vsomeip's SD subsystem decides our service is
/// available. Substring match — exact format is
/// `[subscriber] AVAILABLE service=0x1234 instance=0x1`.
const AVAILABILITY_MARKER: &str = "[subscriber] AVAILABLE service=0x1234";

/// Verifies simple-someip's `Server::announcement_loop` emits SD
/// `OfferService` bytes that vsomeip's reference SD-receive
/// implementation parses + recognizes.
///
/// Test architecture: simple-someip's tokio Server runs the SD
/// announcement loop on the configured interface. A separate
/// vsomeip subscriber container (`vsomeip-test-subscriber`) is
/// already running and has registered an availability handler for
/// service 0x1234 instance 0x0001. When vsomeip's SD subsystem
/// decodes our SD broadcast and decides the service is available,
/// the C++ availability handler prints a marker to stdout. The
/// test polls `docker logs <container>` for that marker.
///
/// `#[ignore]` because this depends on an external vsomeip
/// subscriber container — see module docs for the docker run
/// command.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires external vsomeip-test-subscriber container; see module docs"]
async fn vsomeip_sees_simple_someip_offer_service() {
    let _ = tracing_subscriber::fmt::try_init();

    let interface = test_interface();
    eprintln!("[test] simple-someip Server emitting SD on {interface}");
    eprintln!(
        "[test] expecting vsomeip subscriber to log AVAILABLE for \
         service=0x{SERVICE_ID:04X} instance=0x{INSTANCE_ID:04X} \
         within {}s",
        SD_TIMEOUT.as_secs()
    );

    // Pre-flight: confirm the subscriber container is running so a
    // missing container surfaces as a clear error rather than a
    // 30-second timeout. This isn't bulletproof — the container
    // could die mid-test — but it catches the common "forgot to
    // start it" mistake.
    let pre = std::process::Command::new("docker")
        .args([
            "inspect",
            "--format",
            "{{.State.Running}}",
            SUBSCRIBER_CONTAINER,
        ])
        .output()
        .expect("docker CLI not available; install docker or skip this test");
    if !pre.status.success() {
        panic!(
            "Subscriber container '{SUBSCRIBER_CONTAINER}' not found. \
             Start it via:\n\n  \
             docker run --rm -d --name {SUBSCRIBER_CONTAINER} --network host \\\n    \
             -e VSOMEIP_UNICAST=<your iface IP> -e VSOMEIP_ROLE=subscriber \\\n    \
             vsomeip-offerer\n",
        );
    }
    let running = String::from_utf8_lossy(&pre.stdout);
    if running.trim() != "true" {
        panic!(
            "Subscriber container '{SUBSCRIBER_CONTAINER}' exists but isn't running \
             (state: '{}'). Inspect via `docker logs {SUBSCRIBER_CONTAINER}`.",
            running.trim()
        );
    }

    // Build a tokio-flavor Server with multicast loopback enabled
    // (matches vsomeip's default; lets a same-host subscriber see
    // our broadcasts even on the actual NIC).
    let config = ServerConfig::new(interface, 30500, SERVICE_ID, INSTANCE_ID);
    let mut server = Server::new_with_loopback(config, true)
        .await
        .expect("Server::new_with_loopback failed (network setup problem?)");

    // `announcement_loop()` returns the `+ Send + 'static` future
    // that emits OfferService SD broadcasts every cyclic_offer_delay
    // (default 1s in simple-someip). Spawning it on tokio works
    // here because TokioSocket is Send + Sync and the std-side
    // bounds are met by the convenience constructor's defaults.
    let announce_fut = server
        .announcement_loop()
        .expect("announcement_loop failed; passive server?");
    let announce_handle = tokio::spawn(announce_fut);

    // Drive the server's run loop too — it does multicast-loopback
    // SD receive, but for this test we only care that announcements
    // go out. The run loop survives without subscribers.
    let server_handle = tokio::spawn(async move {
        let _ = server.run().await;
    });

    eprintln!("[test] announcement loop spawned; polling docker logs");

    // Poll docker logs every 500ms for the AVAILABLE marker. Reading
    // the full log each time is fine — they're tiny. Uses
    // `std::process::Command` (blocking) rather than tokio's process
    // module to avoid widening the crate's dev-dep tokio features
    // for one test; the brief blocking call happens between half-
    // second sleeps so it doesn't starve the runtime.
    let saw_marker = tokio::time::timeout(SD_TIMEOUT, async {
        loop {
            let out = std::process::Command::new("docker")
                .args(["logs", SUBSCRIBER_CONTAINER])
                .output();
            if let Ok(o) = out {
                let combined = format!(
                    "{}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                );
                if combined.contains(AVAILABILITY_MARKER) {
                    return true;
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await;

    announce_handle.abort();
    server_handle.abort();

    match saw_marker {
        Ok(true) => {
            eprintln!(
                "[test] PASS — vsomeip subscriber recognized simple-someip's \
                 OfferService SD broadcast"
            );
        }
        Ok(false) => unreachable!("loop only exits via timeout or marker match"),
        Err(_) => {
            // Final docker logs dump for the operator's debugging.
            let logs = std::process::Command::new("docker")
                .args(["logs", "--tail", "30", SUBSCRIBER_CONTAINER])
                .output()
                .ok()
                .map(|o| {
                    format!(
                        "stdout:\n{}\n\nstderr:\n{}",
                        String::from_utf8_lossy(&o.stdout),
                        String::from_utf8_lossy(&o.stderr)
                    )
                })
                .unwrap_or_else(|| "<docker logs unavailable>".to_string());
            panic!(
                "Timed out after {}s waiting for vsomeip subscriber to log \n\
                 '{AVAILABILITY_MARKER}'. Possibilities (rough order of likelihood): \n\
                 (1) simple-someip's announcement_loop isn't actually emitting on \n\
                     {interface} — check tcpdump or RUST_LOG=debug; \n\
                 (2) vsomeip's `unicast` doesn't match the test's interface — \n\
                     set VSOMEIP_UNICAST and SIMPLE_SOMEIP_TEST_INTERFACE the same; \n\
                 (3) wire-format mismatch in simple-someip's SD-emit path — \n\
                     this is the genuine conformance bug case. Try the RX-direction \n\
                     test (`client_sees_vsomeip_offer_service`) to triangulate; \n\
                 (4) vsomeip subscriber crashed mid-test. \n\n\
                 Last 30 lines of subscriber logs:\n{logs}",
                SD_TIMEOUT.as_secs(),
            );
        }
    }
}

// ── Phase 20h: TX direction — wire-format self-check (no docker) ──────

/// Verifies `Server::announcement_loop` emits SOME/IP-SD bytes that
/// match the AUTOSAR SOME/IP-SD spec, by capturing the bytes on a
/// second multicast socket and asserting every field of the SOME/IP +
/// SD envelope.
///
/// **No external reference impl is involved.** This test asserts
/// against the spec, not against vsomeip. The cross-impl validation
/// lives in `vsomeip_sees_simple_someip_offer_service` above (gated
/// on a docker container + ideally a second host); this test gives
/// CI a deterministic, dep-free signal that the emit path is healthy.
///
/// The receive-side cross-impl path is already exercised by
/// `client_sees_vsomeip_offer_service`: vsomeip's emitter feeds
/// simple-someip's parser, and that test passes. So if our parser
/// (vsomeip-compatible by that test) decodes our emitter's bytes
/// with the expected field values here, our emitter is vsomeip-
/// shaped by transitivity. Modulo encoding subtleties not visible to
/// the parser — which is what the docker-based test is for.
///
/// `#[ignore]` because the chosen interface needs the `MULTICAST`
/// flag. Linux's `lo` lacks it by default (`ip link show lo` does
/// not list `MULTICAST`), so this test is run on demand against a
/// real NIC via `SIMPLE_SOMEIP_TEST_INTERFACE=<iface IP>`.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires MULTICAST flag on the chosen interface; pass \
            SIMPLE_SOMEIP_TEST_INTERFACE=<iface IP>. See module docs."]
async fn tx_announcement_loop_emits_wire_format_offer() {
    use std::net::{IpAddr, SocketAddr};

    let _ = tracing_subscriber::fmt::try_init();

    let interface = test_interface();
    eprintln!(
        "[test] capturing simple-someip's SD on {interface}; expecting \
         OfferService(service=0x{SERVICE_ID:04X}, instance=0x{INSTANCE_ID:04X})"
    );

    // Receiver socket: bind to the SD multicast port on `interface`,
    // SO_REUSEPORT so it coexists with the Server's own SD socket
    // (also bound to that port), join the SD multicast group, and
    // enable multicast loopback so a same-host sender's packets
    // reach us.
    let rx = {
        let raw = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )
        .expect("socket2 create");
        raw.set_reuse_address(true).expect("set_reuse_address");
        raw.set_reuse_port(true).expect("set_reuse_port");
        raw.set_multicast_loop_v4(true)
            .expect("set_multicast_loop_v4");
        // Bind to 0.0.0.0:30490, not interface:30490: Linux only
        // delivers multicast to sockets bound to INADDR_ANY (or to
        // the multicast group address itself), not to ones bound to
        // a specific unicast address — even after `join_multicast_v4`.
        // The `join` call below specifies which interface to join on.
        raw.bind(&SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), sd::MULTICAST_PORT).into())
            .expect("bind receiver to 0.0.0.0:SD_PORT");
        raw.set_nonblocking(true).expect("set_nonblocking");
        let std_sock: std::net::UdpSocket = raw.into();
        let sock = tokio::net::UdpSocket::from_std(std_sock).expect("UdpSocket::from_std");
        sock.join_multicast_v4(sd::MULTICAST_IP, interface)
            .expect("join SD multicast group");
        sock
    };

    // Spawn the Server with multicast loopback so its emitted
    // OfferService packets loop back to our receiver on the same
    // interface.
    const ADVERTISED_PORT: u16 = 30500;
    let config = ServerConfig::new(interface, ADVERTISED_PORT, SERVICE_ID, INSTANCE_ID);
    let mut server = Server::new_with_loopback(config, true)
        .await
        .expect("Server::new_with_loopback failed");
    let announce_fut = server
        .announcement_loop()
        .expect("announcement_loop failed; passive server?");
    let announce_handle = tokio::spawn(announce_fut);
    // Drive run() too so the Server's own SD socket drains, but we
    // assert against bytes we receive on our independent capture
    // socket — the run-loop is just to keep the Server healthy.
    let server_handle = tokio::spawn(async move {
        let _ = server.run().await;
    });

    // Owned snapshot of the assertion-relevant fields. Pulled out
    // inside `recv_loop` because `MessageView` / `SdHeaderView` /
    // `EntryView` borrow the receive buffer.
    struct CapturedOffer {
        someip_service_id: u16,
        someip_method_id: u16,
        message_type: MessageType,
        return_code: ReturnCode,
        protocol_version: u8,
        interface_version: u8,
        sd_unicast: bool,
        entry_service_id: u16,
        entry_instance_id: u16,
        entry_major_version: u8,
        entry_minor_version: u32,
        entry_ttl: u32,
        endpoint_ip: Ipv4Addr,
        endpoint_port: u16,
        endpoint_protocol: TransportProtocol,
        len: usize,
    }

    // Cyclic offer delay defaults to ~1 s; 5 s is generous and
    // bounded.
    let recv_timeout = Duration::from_secs(5);
    let recv_loop = async {
        let mut buf = [0u8; 2048];
        loop {
            let (len, _from) = rx.recv_from(&mut buf).await.expect("recv_from");
            let Ok(view) = MessageView::parse(&buf[..len]) else {
                continue;
            };
            if view.header().message_id().service_id() != 0xFFFF {
                continue;
            }
            let Ok(sd_view) = view.sd_header() else {
                continue;
            };
            let Some(entry) = sd_view.entries().next() else {
                continue;
            };
            if !matches!(entry.entry_type(), Ok(EntryType::OfferService)) {
                continue;
            }
            if entry.service_id() != SERVICE_ID {
                continue;
            }
            let first_option = sd_view
                .options()
                .next()
                .expect("OfferService should carry an endpoint option");
            let (endpoint_ip, endpoint_protocol, endpoint_port) = first_option
                .as_ipv4()
                .expect("endpoint option should decode as IPv4");
            return CapturedOffer {
                someip_service_id: view.header().message_id().service_id(),
                someip_method_id: view.header().message_id().method_id(),
                message_type: view.header().message_type().message_type(),
                return_code: view.header().return_code(),
                protocol_version: view.header().protocol_version(),
                interface_version: view.header().interface_version(),
                sd_unicast: sd_view.flags().unicast(),
                entry_service_id: entry.service_id(),
                entry_instance_id: entry.instance_id(),
                entry_major_version: entry.major_version(),
                entry_minor_version: entry.minor_version(),
                entry_ttl: entry.ttl(),
                endpoint_ip,
                endpoint_port,
                endpoint_protocol,
                len,
            };
        }
    };
    let captured = tokio::time::timeout(recv_timeout, recv_loop).await;

    announce_handle.abort();
    server_handle.abort();

    let offer = captured.unwrap_or_else(|_| {
        panic!(
            "Timed out after {}s waiting to capture our own OfferService on \
             {interface}. Most likely cause: `lo` lacks the MULTICAST flag, \
             or SIMPLE_SOMEIP_TEST_INTERFACE points to an interface that \
             cannot loop multicast back to a same-host receiver. Try a \
             real NIC IP (`ip route get 239.255.0.255` to find one).",
            recv_timeout.as_secs(),
        )
    });

    // SOME/IP envelope (spec-fixed for SD).
    assert_eq!(offer.someip_service_id, 0xFFFF, "SD service_id");
    assert_eq!(offer.someip_method_id, 0x8100, "SD method_id");
    assert_eq!(offer.message_type, MessageType::Notification);
    assert_eq!(offer.return_code, ReturnCode::Ok);
    assert_eq!(offer.protocol_version, 0x01);
    assert_eq!(offer.interface_version, 0x01);
    // SD flags — unicast must always be set; reboot may be either
    // RecentlyRebooted or Continuous depending on session counter
    // wrap state, so we don't assert it here (covered by the inner
    // sd_state tests).
    assert!(offer.sd_unicast, "SD unicast flag must be set");
    // OfferService entry body.
    assert_eq!(offer.entry_service_id, SERVICE_ID);
    assert_eq!(offer.entry_instance_id, INSTANCE_ID);
    assert_eq!(offer.entry_major_version, 1, "default major_version");
    assert_eq!(offer.entry_minor_version, 0, "default minor_version");
    assert!(offer.entry_ttl > 0, "TTL must be non-zero on Offer");
    // Endpoint option — must advertise the configured (interface, port)
    // pair as UDP, which is what vsomeip's parser scans for.
    assert_eq!(offer.endpoint_ip, interface);
    assert_eq!(offer.endpoint_port, ADVERTISED_PORT);
    assert_eq!(offer.endpoint_protocol, TransportProtocol::Udp);

    eprintln!(
        "[test] PASS — captured wire-format OfferService for service=0x{SERVICE_ID:04X} \
         on {interface} ({len} bytes)",
        len = offer.len
    );
    // `RebootFlag` is referenced via the trace-friendly Display path
    // implicitly by tracing; pin the import so it's not flagged.
    let _ = RebootFlag::RecentlyRebooted;
}
