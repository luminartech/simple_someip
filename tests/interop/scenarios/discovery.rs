//! Service discovery scenarios.

use crate::Rt;
use crate::interop::consts::*;
use crate::interop::peers::vsomeip::VsomeipPeer;
use crate::interop::runtime::{Consume, Observation, Offer, Setup, SomeipUnderTest};

use super::SD_WAIT;

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
    bare_metal = run,
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
        peer.expect_where("AVAILABLE", SD_WAIT, |l| l.hex("service") == u32::from(SVC));
    }
);
