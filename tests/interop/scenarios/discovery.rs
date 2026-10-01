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

use super::{SD_WAIT, TTL_WAIT};

fn offer() -> Offer {
    Offer {
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
    bare_metal = ignore("the bare-metal runtime does not report discovered services"),
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
fn first_offer(fp: &FramePeer) -> Vec<u8> {
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
    std = run,
    bare_metal = ignore("the bare-metal runtime does not report discovered services"),
    {
        let mut peer = VsomeipPeer::start();
        let mut rt = Rt::start(consume_only());
        peer.send("offer 1234 0001 1 0001 8001 8002 0001");
        rt.expect("ServiceAvailable", SD_WAIT, is_svc_available);
        peer.send("stop-offer 1234 0001");
        rt.expect("ServiceGone", SD_WAIT, is_svc_gone);
    }
);

scenario!(
    /// PRS_SOMEIPSD_00842, 00364 / feat_req_someipsd_208 — a StopOffer (type 0x01, TTL 0) from a hand-built frame removes the service.
    d3_stop_offer_removes_service_frame,
    std = run,
    bare_metal = ignore("the bare-metal runtime does not report discovered services"),
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
    /// PRS_SOMEIPSD_00842, 00364 — our StopOffer is understood by vsomeip.
    d4_our_stop_offer_is_understood,
    std = ignore("the std runtime cannot stop offering a service"),
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
    /// PRS_SOMEIPSD_00268 (index of the first options run) — each Offer in one message uses the endpoint it references.
    d5_each_offer_uses_its_own_endpoint,
    std = run,
    bare_metal = ignore("the bare-metal runtime does not report discovered services"),
    {
        let fp = FramePeer::start();
        let mut rt = Rt::start(consume_only());
        let other = build::service_entry(OFFER_SERVICE, 1, 0, 1, 0, OTHER_SVC, INST, MAJOR, 3, 0);
        let other_ep = build::ipv4_endpoint_option(PEER_IP, UDP, OTHER_PORT);
        fp.send_sd_multicast(&build::sd_message(
            1,
            true,
            true,
            &[svc_offer(0, 1, 3), other],
            &[peer_endpoint(), other_ep],
        ));
        let endpoint_of = |o: &Observation, service: u16| match o {
            Observation::ServiceAvailable {
                service: s,
                endpoint,
                ..
            } if *s == service => Some(*endpoint),
            _ => None,
        };
        let svc = rt.expect("ServiceAvailable for 0x1234", SD_WAIT, |o| {
            endpoint_of(o, SVC).is_some()
        });
        let other = rt.expect("ServiceAvailable for 0x5678", SD_REPLY, |o| {
            endpoint_of(o, OTHER_SVC).is_some()
        });
        let (svc, other) = (endpoint_of(&svc, SVC), endpoint_of(&other, OTHER_SVC));
        assert_eq!(
            (svc, other),
            (
                Some(SocketAddrV4::new(PEER_IP, SERVER_PORT)),
                Some(SocketAddrV4::new(PEER_IP, OTHER_PORT))
            ),
            "{}: the two offers' endpoints (0x1234, 0x5678)",
            Rt::NAME
        );
    }
);

scenario!(
    /// PRS_SOMEIPSD_00231 / feat_req_someipsd_1142 — an Offer still applies when it also references a discardable option of unknown type.
    d6_unknown_option_is_skipped_client,
    std = run,
    bare_metal = ignore("the bare-metal runtime does not report discovered services"),
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
    std = run,
    bare_metal = run,
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
    /// PRS_SOMEIPSD_00842 (supported entry types) — an Offer still applies when it follows an entry of unknown type.
    d7_unknown_entry_type_is_skipped_client,
    std = run,
    bare_metal = ignore("the bare-metal runtime does not report discovered services"),
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
    /// PRS_SOMEIPSD_00842 (supported entry types) — a Subscribe still gets an Ack when it follows an entry of unknown type.
    d7_unknown_entry_type_is_skipped_server,
    std = run,
    bare_metal = run,
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
    std = run,
    bare_metal = ignore("the bare-metal runtime does not report discovered services"),
    {
        let fp = FramePeer::start();
        let mut rt = Rt::start(consume_only());
        let offer = || [svc_offer(0, 1, 3)];
        fp.send_sd_multicast(&build::sd_message(5, true, true, &offer(), &[peer_endpoint()]));
        rt.expect("ServiceAvailable", SD_WAIT, is_svc_available);
        // Reboot flag set both times and the session ID going back: a reboot.
        fp.send_sd_multicast(&build::sd_message(1, true, true, &offer(), &[peer_endpoint()]));
        rt.expect("ServiceGone after the reboot", SD_REPLY, is_svc_gone);
        rt.expect("ServiceAvailable after the reboot", SD_REPLY, is_svc_available);
    }
);

scenario!(
    /// PRS_SOMEIPSD_00422 / feat_req_someipsd_824, feat_req_someipsd_91 — a Find with the unicast flag 0 is answered with an Offer.
    d9_find_without_unicast_flag_is_answered,
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
            &build::sd_message(1, true, false, &[find], &[]),
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
    std = run,
    bare_metal = ignore("the bare-metal runtime does not report discovered services"),
    {
        let fp = FramePeer::start();
        let mut rt = Rt::start(consume_only());
        fp.send_sd_multicast(&build::sd_message(1, true, true, &[svc_offer(0, 1, 3)], &[peer_endpoint()]));
        rt.expect("ServiceAvailable", SD_WAIT, is_svc_available);
        rt.expect("ServiceGone once the 3 s TTL expires", TTL_WAIT, is_svc_gone);
    }
);

scenario!(
    /// PRS_SOMEIPSD_00158 — the first SD message's session ID is 1.
    d11_first_sd_session_id_is_one,
    std = run,
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
