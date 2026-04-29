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
//! 1. Pull or build a vsomeip container. The COVESA project doesn't
//!    publish a "ready-to-go" image; the simplest path is a small
//!    Dockerfile around vsomeip's cmake build. The image needs
//!    `routingmanagerd` (the SD daemon) plus a JSON config that
//!    declares an "offerer" application with the service we want
//!    advertised. Phase 20g will add a reference Dockerfile under
//!    `tests/data/vsomeip-offerer/` once the manual setup is
//!    proven; until then, hand-rolled is fine.
//!
//! 2. Save the config below as `vsomeip-offerer.json` and start
//!    the container in host-network mode so SD multicast (224.0.23.0)
//!    flows between the host and the container:
//!
//!    ```text
//!    docker run --rm -d \
//!      --name vsomeip-offerer \
//!      --network host \
//!      -v $(pwd)/vsomeip-offerer.json:/etc/vsomeip.json:ro \
//!      -e VSOMEIP_CONFIGURATION=/etc/vsomeip.json \
//!      -e VSOMEIP_APPLICATION_NAME=offerer \
//!      <your vsomeip image>
//!    ```
//!
//!    Sample vsomeip-offerer.json that offers service 0x1234
//!    instance 0x0001 over UDP port 30509:
//!
//!    ```json
//!    {
//!      "unicast": "127.0.0.1",
//!      "logging": { "level": "info", "console": "true" },
//!      "applications": [
//!        { "name": "offerer", "id": "0x1277" }
//!      ],
//!      "services": [
//!        {
//!          "service": "0x1234",
//!          "instance": "0x0001",
//!          "unreliable": "30509"
//!        }
//!      ],
//!      "routing": "offerer",
//!      "service-discovery": {
//!        "enable": "true",
//!        "multicast": "224.0.23.0",
//!        "port": "30490",
//!        "protocol": "udp",
//!        "initial_delay_min": "10",
//!        "initial_delay_max": "100",
//!        "repetitions_base_delay": "200",
//!        "repetitions_max": "3",
//!        "ttl": "5"
//!      }
//!    }
//!    ```
//!
//! 3. Set the test's listening interface via env var to whatever IP
//!    vsomeip is announcing on. For host-network Docker, that's
//!    typically `127.0.0.1` (matches `unicast` in the config above):
//!
//!    ```text
//!    SIMPLE_SOMEIP_TEST_INTERFACE=127.0.0.1 \
//!    cargo test --features client-tokio,server-tokio \
//!      --test vsomeip_sd_compat -- --ignored --nocapture
//!    ```
//!
//! 4. Tear down: `docker stop vsomeip-offerer`.
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

use simple_someip::{Client, ClientUpdate, RawPayload};

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
