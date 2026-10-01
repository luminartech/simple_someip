//! Service discovery scenarios.

use std::net::SocketAddrV4;
use std::time::{Duration, Instant};

use crate::Rt;
use crate::interop::consts::*;
use crate::interop::peers::frames::{
    Delivery, FIND_SERVICE, FramePeer, OFFER_SERVICE, SUBSCRIBE_EVENTGROUP,
    SUBSCRIBE_EVENTGROUP_ACK, UDP, build, parse,
};
use crate::interop::peers::vsomeip::VsomeipPeer;
use crate::interop::runtime::{Consume, Observation, Offer, Setup, SomeipUnderTest};

use super::{QUIET, SD_WAIT, TTL_WAIT};

fn offer() -> Offer {
    Offer {
        service: SVC,
        ttl_s: 3,
        events: vec![EVENT],
        fields: vec![(FIELD, vec![0x2A])],
        methods: vec![METHOD],
    }
}

scenario!(
    /// PRS_SOMEIPSD_00842 (OfferService) / feat_req_someipsd_208 — the peer's offer is discovered.
    d1_peer_offer_is_discovered,
    std = run,
    bare_metal = ignore("the bare-metal runtime does not report discovered services and cannot start without offering one (#192, #193)"),
    {
        let mut peer = VsomeipPeer::start();
        let mut rt = Rt::start(Setup {
            offer: None,
            consume: Some(Consume {
                subscribe: false,
                e2e: None,
            }),
        });
        peer.send("offer 1234 0001 1 0001 8001 8002 0001");
        rt.expect("ServiceAvailable", SD_WAIT, |o| {
            matches!(o,
                Observation::ServiceAvailable { service: SVC, instance: INST, endpoint }
                    if endpoint.ip() == &PEER_IP && endpoint.port() == SERVER_PORT)
        });
    }
);

scenario!(
    /// PRS_SOMEIPSD_00842 (OfferService) — our offer is discovered by the peer.
    d2_our_offer_is_discovered,
    std = run,
    bare_metal = run,
    {
        let mut peer = VsomeipPeer::start();
        peer.send("require 1234 0001 1");
        let _rt = Rt::start(Setup {
            offer: Some(offer()),
            consume: None,
        });
        peer.expect_where("AVAILABLE", SD_WAIT, |l| {
            l.hex("service") == u32::from(SVC) && l.hex("instance") == u32::from(INST)
        });
    }
);

/// How long a reply to one SD message may take.
const SD_REPLY: Duration = Duration::from_secs(2);

/// A second service, offered alongside `SVC`.
const OTHER_SVC: u16 = 0x5678;
/// The second service's port.
const OTHER_PORT: u16 = 30600;

/// An option type that no specification defines.
const UNKNOWN_OPTION: u8 = 0x77;
/// An entry type that no specification defines.
const UNKNOWN_ENTRY: u8 = 0x42;

fn consume_only() -> Setup {
    Setup {
        offer: None,
        consume: Some(Consume {
            subscribe: false,
            e2e: None,
        }),
    }
}

fn offer_only() -> Setup {
    Setup {
        offer: Some(offer()),
        consume: None,
    }
}

/// The session ID in a SOME/IP header.
fn session(d: &[u8]) -> u16 {
    u16::from_be_bytes([d[10], d[11]])
}

/// The frame peer's endpoint option for `SVC`.
fn peer_endpoint() -> Vec<u8> {
    build::ipv4_endpoint_option(PEER_IP, UDP, SERVER_PORT)
}

/// An Offer for `SVC` referencing the options from `idx1`, `n1` of them.
fn svc_offer(idx1: u8, n1: u8, ttl: u32) -> [u8; 16] {
    build::service_entry(OFFER_SERVICE, idx1, 0, n1, 0, SVC, INST, MAJOR, ttl, 0)
}

fn is_svc_available(o: &Observation) -> bool {
    matches!(o, Observation::ServiceAvailable { service: SVC, .. })
}

fn is_svc_gone(o: &Observation) -> bool {
    matches!(o, Observation::ServiceGone { service: SVC, .. })
}

/// Waits for the runtime's first OfferService, so it is known to be running.
pub(super) fn first_offer(fp: &FramePeer) -> Vec<u8> {
    let (d, _, _) = fp
        .recv_sd(SD_WAIT, |d| {
            parse::sd_entry_types(d)
                .iter()
                .any(|&(t, ttl)| t == OFFER_SERVICE && ttl > 0)
        })
        .unwrap_or_else(|| panic!("{}: no OfferService within {SD_WAIT:?}", Rt::NAME));
    d
}

/// Sends `entries` and `options` to the runtime's SD port and returns the
/// TTL of the SubscribeEventgroupAck entry that answers them (0 for a Nack).
fn subscribe_and_wait_for_ack(fp: &FramePeer, entries: &[[u8; 16]], options: &[Vec<u8>]) -> u32 {
    let is_ack = |&(t, _): &(u8, u32)| t == SUBSCRIBE_EVENTGROUP_ACK;
    fp.send_sd_unicast(
        SocketAddrV4::new(OUR_IP, SD_PORT),
        &build::sd_message(1, true, true, entries, options),
    );
    let (d, _, _) = fp
        .recv_sd(SD_REPLY, |d| parse::sd_entry_types(d).iter().any(is_ack))
        .unwrap_or_else(|| {
            panic!(
                "{}: no SubscribeEventgroupAck within {SD_REPLY:?}",
                Rt::NAME
            )
        });
    let (_, ttl) = parse::sd_entry_types(&d)
        .into_iter()
        .find(is_ack)
        .expect("the predicate matched an Ack");
    ttl
}

/// A Subscribe for `EG` referencing the options from `idx1`, `n1` of them.
fn eg_subscribe(idx1: u8, n1: u8) -> [u8; 16] {
    build::eventgroup_entry(
        SUBSCRIBE_EVENTGROUP,
        idx1,
        n1,
        SVC,
        INST,
        MAJOR,
        3,
        false,
        0,
        EG,
    )
}

scenario!(
    /// PRS_SOMEIPSD_00842, 00364 / feat_req_someipsd_208 — a StopOffer (type 0x01, TTL 0) from vsomeip removes the service.
    d3_stop_offer_removes_service_vsomeip,
    std = ignore("a StopOffer (entry type 0x01, TTL 0) must remove the service (#166)"),
    bare_metal = ignore("the bare-metal runtime does not report discovered services and cannot start without offering one (#192, #193)"),
    {
        let mut peer = VsomeipPeer::start();
        let mut rt = Rt::start(consume_only());
        peer.send("offer 1234 0001 1 0001 8001 8002 0001");
        rt.expect("ServiceAvailable", SD_WAIT, is_svc_available);
        peer.send("stop-offer 1234 0001");
        // Well inside vsomeip's 3 s offer TTL, so only the StopOffer explains
        // the service going.
        rt.expect("ServiceGone", QUIET, is_svc_gone);
    }
);

scenario!(
    /// PRS_SOMEIPSD_00842, 00364 / feat_req_someipsd_208 — a StopOffer (type 0x01, TTL 0) from a hand-built frame removes the service.
    d3_stop_offer_removes_service_frame,
    std = ignore("a StopOffer (entry type 0x01, TTL 0) must remove the service (#166)"),
    bare_metal = ignore("the bare-metal runtime does not report discovered services and cannot start without offering one (#192, #193)"),
    {
        let fp = FramePeer::start();
        let mut rt = Rt::start(consume_only());
        let offer = svc_offer(0, 1, 3);
        fp.send_sd_multicast(&build::sd_message(1, true, true, &[offer], &[peer_endpoint()]));
        rt.expect("ServiceAvailable", SD_WAIT, is_svc_available);
        let stop = svc_offer(0, 1, 0);
        fp.send_sd_multicast(&build::sd_message(2, true, true, &[stop], &[peer_endpoint()]));
        rt.expect("ServiceGone", SD_REPLY, is_svc_gone);
    }
);

scenario!(
    /// PRS_SOMEIPSD_00427 — vsomeip drops our service when we stop offering it.
    d4_our_stop_offer_is_understood,
    std = ignore("the std runtime cannot stop offering a service (#173)"),
    bare_metal = run,
    {
        let mut peer = VsomeipPeer::start();
        peer.send("require 1234 0001 1");
        // A TTL far longer than the wait below, so only a StopOffer explains
        // the peer losing the service.
        let mut rt = Rt::start(Setup {
            offer: Some(Offer {
                ttl_s: 30,
                ..offer()
            }),
            consume: None,
        });
        peer.expect_where("AVAILABLE", SD_WAIT, |l| l.hex("service") == u32::from(SVC));
        peer.clear();
        rt.stop_offer();
        peer.expect_where("UNAVAILABLE", Duration::from_millis(1500), |l| {
            l.hex("service") == u32::from(SVC)
        });
    }
);

scenario!(
    /// PRS_SOMEIPSD_00842, 00364, 00157, 00255 / feat_req_someipsd_208 — our StopOffer is an Offer entry with TTL 0 that continues the offers' session and reboot flag.
    d4_our_stop_offer_wire_format,
    std = ignore("the std runtime cannot stop offering a service (#173)"),
    bare_metal = ignore("a StopOffer must be entry type 0x01 with TTL 0 and continue the offers' session ID and reboot flag (#166, #171)"),
    {
        let fp = FramePeer::start();
        let mut rt = Rt::start(offer_only());
        let mut last = first_offer(&fp);
        rt.stop_offer();
        // The first SD message with a TTL 0 entry; any offer before it
        // becomes the one the stop must follow.
        let deadline = Instant::now() + SD_REPLY;
        let stop = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let (d, _, _) = fp.recv_sd(left, |_| true).unwrap_or_else(|| {
                panic!(
                    "{}: no SD entry with TTL 0 within {SD_REPLY:?} of stopping",
                    Rt::NAME
                )
            });
            let entries = parse::sd_entry_types(&d);
            if entries.iter().any(|&(_, ttl)| ttl == 0) {
                break d;
            }
            if entries.iter().any(|&(t, _)| t == OFFER_SERVICE) {
                last = d;
            }
        };

        let mut problems = Vec::new();
        let entries = parse::sd_entry_types(&stop);
        if entries != [(OFFER_SERVICE, 0)] {
            problems.push(format!(
                "entries (type, TTL): got {entries:02X?}, want [(01, 00)]"
            ));
        }
        let reboot = |d: &[u8]| d[16] & 0x80 != 0;
        // A handful of messages cannot wrap the session counter.
        if !reboot(&stop) {
            problems.push("reboot flag: got clear, want set".to_owned());
        }
        if reboot(&stop) != reboot(&last) {
            problems.push(format!(
                "reboot flag: {} on the stop but {} on the last offer",
                reboot(&stop),
                reboot(&last)
            ));
        }
        let want = session(&last).wrapping_add(1).max(1);
        if session(&stop) != want {
            problems.push(format!(
                "session ID: got {:#06X}, want {want:#06X} (the last offer's was {:#06X})",
                session(&stop),
                session(&last)
            ));
        }
        assert!(
            problems.is_empty(),
            "{}: our StopOffer does not match the SD wire format:\n  {}\nlast offer: {last:02X?}\nstop:       {stop:02X?}",
            Rt::NAME,
            problems.join("\n  ")
        );
    }
);

scenario!(
    /// PRS_SOMEIPSD_00268 (index of the first options run) — each Offer in one message uses the endpoint it references.
    d5_each_offer_uses_its_own_endpoint,
    std = ignore("each Offer must use the endpoint option it references (#168)"),
    bare_metal = ignore("the bare-metal runtime does not report discovered services and cannot start without offering one (#192, #193)"),
    {
        let fp = FramePeer::start();
        let mut rt = Rt::start(consume_only());
        // Crossed: entry 0 uses option 1 and entry 1 uses option 0, so
        // neither "the first option for every entry" nor "entry i uses
        // option i" gives both services the right endpoint.
        let other = build::service_entry(OFFER_SERVICE, 0, 0, 1, 0, OTHER_SVC, INST, MAJOR, 3, 0);
        let other_ep = build::ipv4_endpoint_option(PEER_IP, UDP, OTHER_PORT);
        fp.send_sd_multicast(&build::sd_message(
            1,
            true,
            true,
            &[svc_offer(1, 1, 3), other],
            &[other_ep, peer_endpoint()],
        ));
        // The first ServiceAvailable for each service, in whatever order.
        let mut found: [Option<SocketAddrV4>; 2] = [None, None];
        let mut skipped = Vec::new();
        let deadline = Instant::now() + SD_WAIT;
        while found.contains(&None) {
            let left = deadline.saturating_duration_since(Instant::now());
            let Some(o) = rt.next(left) else { break };
            match o {
                Observation::ServiceAvailable {
                    service: SVC,
                    endpoint,
                    ..
                } if found[0].is_none() => found[0] = Some(endpoint),
                Observation::ServiceAvailable {
                    service: OTHER_SVC,
                    endpoint,
                    ..
                } if found[1].is_none() => found[1] = Some(endpoint),
                o => skipped.push(format!("{o:?}")),
            }
        }
        assert_eq!(
            found,
            [
                Some(SocketAddrV4::new(PEER_IP, SERVER_PORT)),
                Some(SocketAddrV4::new(PEER_IP, OTHER_PORT))
            ],
            "{}: the endpoints of 0x1234 and 0x5678 (None: not reported within {SD_WAIT:?}); \
             other observations: {skipped:?}",
            Rt::NAME
        );
    }
);

scenario!(
    /// PRS_SOMEIPSD_00231 / feat_req_someipsd_1142 — an Offer still applies when it also references a discardable option of unknown type.
    d6_unknown_option_is_skipped_client,
    std = ignore("an unknown discardable option must be ignored, not reject the SD message (#167)"),
    bare_metal = ignore("the bare-metal runtime does not report discovered services and cannot start without offering one (#192, #193)"),
    {
        let fp = FramePeer::start();
        let mut rt = Rt::start(consume_only());
        let unknown = build::discardable_option(UNKNOWN_OPTION, &[0; 4]);
        fp.send_sd_multicast(&build::sd_message(
            1,
            true,
            true,
            &[svc_offer(0, 2, 3)],
            &[unknown, peer_endpoint()],
        ));
        rt.expect("ServiceAvailable on port 30509", SD_WAIT, |o| {
            matches!(o, Observation::ServiceAvailable { service: SVC, endpoint, .. }
                if endpoint.port() == SERVER_PORT)
        });
    }
);

scenario!(
    /// PRS_SOMEIPSD_00231 / feat_req_someipsd_1142 — a Subscribe still gets an Ack when it also references a discardable option of unknown type.
    d6_unknown_option_is_skipped_server,
    std = ignore("an unknown discardable option must be ignored, not reject the SD message (#167)"),
    bare_metal = ignore("an unknown discardable option must be ignored, not reject the SD message (#167)"),
    {
        let fp = FramePeer::start();
        let _rt = Rt::start(offer_only());
        first_offer(&fp);
        let unknown = build::discardable_option(UNKNOWN_OPTION, &[0; 4]);
        let ttl = subscribe_and_wait_for_ack(&fp, &[eg_subscribe(0, 2)], &[unknown, peer_endpoint()]);
        assert!(ttl > 0, "{}: the Subscribe was answered with a Nack", Rt::NAME);
    }
);

scenario!(
    /// PRS_SOMEIPSD_00841 — an Offer still applies when it follows an entry of unknown type.
    d7_unknown_entry_type_is_skipped_client,
    std = ignore("an SD entry of unknown type must be ignored, not reject the SD message (#167)"),
    bare_metal = ignore("the bare-metal runtime does not report discovered services and cannot start without offering one (#192, #193)"),
    {
        let fp = FramePeer::start();
        let mut rt = Rt::start(consume_only());
        let unknown = build::service_entry(UNKNOWN_ENTRY, 0, 0, 0, 0, SVC, INST, MAJOR, 3, 0);
        fp.send_sd_multicast(&build::sd_message(
            1,
            true,
            true,
            &[unknown, svc_offer(0, 1, 3)],
            &[peer_endpoint()],
        ));
        rt.expect("ServiceAvailable", SD_WAIT, is_svc_available);
    }
);

scenario!(
    /// PRS_SOMEIPSD_00841 — a Subscribe still gets an Ack when it follows an entry of unknown type.
    d7_unknown_entry_type_is_skipped_server,
    std = ignore("an SD entry of unknown type must be ignored, not reject the SD message (#167)"),
    bare_metal = ignore("an SD entry of unknown type must be ignored, not reject the SD message (#167)"),
    {
        let fp = FramePeer::start();
        let _rt = Rt::start(offer_only());
        first_offer(&fp);
        let unknown = build::service_entry(UNKNOWN_ENTRY, 0, 0, 0, 0, SVC, INST, MAJOR, 3, 0);
        let ttl = subscribe_and_wait_for_ack(&fp, &[unknown, eg_subscribe(0, 1)], &[peer_endpoint()]);
        assert!(ttl > 0, "{}: the Subscribe was answered with a Nack", Rt::NAME);
    }
);

scenario!(
    /// PRS_SOMEIPSD_00449, 00258 — a detected peer reboot is handled as a StopOffer, then the new Offer applies.
    d8_peer_reboot_is_a_stop_offer,
    std = ignore("a detected server reboot must be handled as a StopOffer (#170)"),
    bare_metal = ignore("the bare-metal runtime does not report discovered services and cannot start without offering one (#192, #193)"),
    {
        let fp = FramePeer::start();
        let mut rt = Rt::start(consume_only());
        let offer = || [svc_offer(0, 1, 3)];
        fp.send_sd_multicast(&build::sd_message(5, true, true, &offer(), &[peer_endpoint()]));
        rt.expect("ServiceAvailable", SD_WAIT, is_svc_available);
        // Control: the session ID moving on is not a reboot.
        fp.send_sd_multicast(&build::sd_message(6, true, true, &offer(), &[peer_endpoint()]));
        rt.expect_none("ServiceGone without a reboot", QUIET, is_svc_gone);
        // Reboot flag set both times and the session ID going back: a reboot.
        fp.send_sd_multicast(&build::sd_message(1, true, true, &offer(), &[peer_endpoint()]));
        rt.expect("ServiceGone after the reboot", SD_REPLY, is_svc_gone);
        rt.expect("ServiceAvailable after the reboot", SD_REPLY, is_svc_available);
    }
);

scenario!(
    /// PRS_SOMEIPSD_00422 / feat_req_someipsd_824 — a Find is answered with an Offer.
    d9_find_is_answered,
    std = run,
    bare_metal = run,
    {
        let fp = FramePeer::start();
        let _rt = Rt::start(offer_only());
        // Right after a cyclic offer, so the next one is a full cycle away.
        let cyclic = first_offer(&fp);
        let find = build::service_entry(FIND_SERVICE, 0, 0, 0, 0, SVC, INST, MAJOR, 3, 0);
        fp.send_sd_unicast(
            SocketAddrV4::new(OUR_IP, SD_PORT),
            &build::sd_message(1, true, true, &[find], &[]),
        );
        let sent = Instant::now();
        let (reply, src, delivery) = fp
            .recv_sd(Duration::from_secs(1), |d| {
                parse::sd_entry_types(d)
                    .iter()
                    .any(|&(t, ttl)| t == OFFER_SERVICE && ttl > 0)
            })
            .unwrap_or_else(|| panic!("{}: no OfferService within 1 s of the Find", Rt::NAME));
        let after = sent.elapsed();
        // A unicast Offer can only be a reply; a multicast one is a reply
        // only if it follows the Find at once.
        assert!(
            delivery == Delivery::Unicast || after <= Duration::from_millis(200),
            "{}: the only Offer after the Find was a {delivery:?} one from {src}, {after:?} later \
             (session {:#06X}; the last cyclic offer's was {:#06X})",
            Rt::NAME,
            session(&reply),
            session(&cyclic)
        );
    }
);

scenario!(
    /// PRS_SOMEIPSD_00356 (Offer TTL) — an Offer that is not renewed expires after its TTL.
    d10_offer_ttl_expiry_removes_service,
    std = ignore("a service must be removed when its Offer's TTL expires (#169)"),
    bare_metal = ignore("the bare-metal runtime does not report discovered services and cannot start without offering one (#192, #193)"),
    {
        let fp = FramePeer::start();
        let mut rt = Rt::start(consume_only());
        fp.send_sd_multicast(&build::sd_message(1, true, true, &[svc_offer(0, 1, 3)], &[peer_endpoint()]));
        let sent = Instant::now();
        rt.expect("ServiceAvailable", SD_WAIT, is_svc_available);
        // Not before the TTL is nearly up...
        let early = Duration::from_millis(2500);
        rt.expect_none(
            "ServiceGone before the 3 s TTL expires",
            early.saturating_sub(sent.elapsed()),
            is_svc_gone,
        );
        // ...but once it has.
        rt.expect(
            "ServiceGone once the 3 s TTL expires",
            TTL_WAIT.saturating_sub(sent.elapsed()),
            is_svc_gone,
        );
    }
);

scenario!(
    /// PRS_SOMEIPSD_00158 — the first SD message's session ID is 1.
    d11_first_sd_session_id_is_one,
    std = ignore("the first SD message must have session ID 1 (#172)"),
    bare_metal = run,
    {
        let fp = FramePeer::start();
        let _rt = Rt::start(offer_only());
        let (d, _, _) = fp
            .recv_sd(SD_WAIT, |d| d.starts_with(&[0xFF, 0xFF, 0x81, 0x00]))
            .unwrap_or_else(|| panic!("{}: no SD message within {SD_WAIT:?}", Rt::NAME));
        assert_eq!(
            session(&d),
            0x0001,
            "{}: first SD message's session ID: {d:02X?}",
            Rt::NAME
        );
    }
);
