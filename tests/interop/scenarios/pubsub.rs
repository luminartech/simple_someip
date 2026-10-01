//! Publish/subscribe scenarios.
//!
//! No scenario here calls `publish` or `set_field` while it waits for an
//! initial value, the adapters never publish on their own, and the peer is
//! only a client of the field. So a notification for `FIELD` can only be an
//! initial value sent by the runtime under test. Each initial-value scenario
//! also checks that no further notification follows it, so a runtime that
//! published the field cyclically could not pass.

use std::net::SocketAddrV4;
use std::time::{Duration, Instant};

use crate::Rt;
use crate::interop::consts::*;
use crate::interop::peers::frames::{
    Delivery, EXPLICIT_INITIAL_DATA_CONTROL, FramePeer, OFFER_SERVICE, REBOOT,
    SUBSCRIBE_EVENTGROUP, SUBSCRIBE_EVENTGROUP_ACK, UDP, UNICAST, build, parse,
};
use crate::interop::peers::vsomeip::{PeerLine, VsomeipPeer};
use crate::interop::runtime::{Consume, Observation, Offer, Setup, SomeipUnderTest};

use super::discovery::first_offer;
use super::{QUIET, SD_WAIT};

/// How long an answer to one SD message may take.
const ACK_WAIT: Duration = Duration::from_secs(2);
/// How soon after the SubscribeEventgroupAck an initial value must arrive.
const INITIAL_WAIT: Duration = Duration::from_secs(1);
/// Long enough for the runtime to handle one SD message.
const SETTLE: Duration = Duration::from_millis(200);
/// The TTL of the Subscribes the frame peer sends.
const SUBSCRIBE_TTL: u32 = 3;
/// How often the frame peer offers when it is the server.
const OFFER_PERIOD: Duration = Duration::from_secs(1);
/// How soon after an Offer the Subscribe answering it must arrive.
const ANSWER_WAIT: Duration = Duration::from_millis(500);
/// When the frame peer sends the Offers after the first Subscribe that
/// must each be answered: about a second apart, but unevenly, so a client
/// renewing on a timer of its own cannot land in both answer windows.
const RENEWING_OFFERS: [Duration; 2] = [Duration::from_millis(800), Duration::from_millis(2100)];

/// A service of the runtime's own that has nothing to do with `SVC`.
const UNRELATED_SVC: u16 = 0x5678;

/// The field's value in the runtime's offer.
const FIELD_VALUE: [u8; 1] = [0x2A];

fn offer() -> Offer {
    Offer {
        service: SVC,
        ttl_s: 3,
        events: vec![EVENT],
        fields: vec![(FIELD, FIELD_VALUE.to_vec())],
        methods: vec![METHOD],
    }
}

/// The runtime as the server of `SVC`.
fn server() -> Setup {
    Setup {
        offer: Some(offer()),
        consume: None,
    }
}

/// The runtime as a client of `SVC`, subscribing to `EG`.
///
/// The bare-metal runtime cannot start without an offer, and a deployed node
/// offers its own services while it subscribes, so it also offers an
/// unrelated service. The std runtime runs a Client alone: a std Server in
/// the same process would bind the SD port too and split unicast SD with
/// the Client.
pub(super) fn client() -> Setup {
    let unrelated = Offer {
        service: UNRELATED_SVC,
        ttl_s: 3,
        events: vec![],
        fields: vec![],
        methods: vec![],
    };
    Setup {
        offer: cfg!(feature = "bare-metal-runtime").then_some(unrelated),
        consume: Some(Consume {
            subscribe: true,
            e2e: None,
        }),
    }
}

/// vsomeip subscribes to `EG` of the runtime's `SVC`; panics unless the
/// subscription is accepted.
fn vsomeip_subscribes(peer: &mut VsomeipPeer) {
    peer.send("require 1234 0001 1");
    peer.send("subscribe 1234 0001 0001 1 8001 8002");
    let line = peer.expect_where("SUBSCRIPTION", SD_WAIT, |l| {
        l.hex("service") == u32::from(SVC) && l.hex("eventgroup") == u32::from(EG)
    });
    assert_eq!(
        line.fields.get("status").map(String::as_str),
        Some("accepted"),
        "{}: vsomeip's subscription was not accepted: {}",
        Rt::NAME,
        line.raw
    );
}

fn is_peer_event(l: &PeerLine, event: u16, payload: &[u8]) -> bool {
    l.hex("event") == u32::from(event) && l.payload() == payload
}

/// Has vsomeip, which offers `SVC`, send `EVENT` with `payload` every
/// 200 ms until the runtime reports it; panics after `SD_WAIT`.
fn notify_until_event(peer: &mut VsomeipPeer, rt: &mut Rt, payload: &[u8]) {
    let hex: String = payload.iter().map(|b| format!("{b:02x}")).collect();
    let deadline = Instant::now() + SD_WAIT;
    let mut skipped = Vec::new();
    while Instant::now() < deadline {
        peer.send(&format!("notify 1234 0001 8001 {hex}"));
        let tick = (Instant::now() + Duration::from_millis(200)).min(deadline);
        while let Some(left) = tick.checked_duration_since(Instant::now()) {
            match rt.next(left) {
                Some(Observation::Event {
                    service: SVC,
                    event: EVENT,
                    payload: p,
                    ..
                }) if p == payload => return,
                Some(o) => skipped.push(format!("{o:?}")),
                None => break,
            }
        }
    }
    panic!(
        "{}: no Event 0x{EVENT:04X} with payload {payload:02X?} within {SD_WAIT:?}; \
         observed: {skipped:?}",
        Rt::NAME
    )
}

/// The frame peer's endpoint option.
fn peer_endpoint() -> Vec<u8> {
    build::ipv4_endpoint_option(PEER_IP, UDP, SERVER_PORT)
}

/// A Subscribe for `EG` referencing the one endpoint option; a `ttl` of 0
/// makes it a StopSubscribe.
fn subscribe_entry(ttl: u32, initial_data_requested: bool) -> [u8; 16] {
    build::eventgroup_entry(
        SUBSCRIBE_EVENTGROUP,
        0,
        1,
        SVC,
        INST,
        MAJOR,
        ttl,
        initial_data_requested,
        0,
        EG,
    )
}

fn entry_ttl(e: &[u8]) -> u32 {
    u32::from_be_bytes([0, e[9], e[10], e[11]])
}

/// Whether `e` is an eventgroup entry of type `entry_type` for `SVC`/`EG`.
fn is_eventgroup_entry(e: &[u8], entry_type: u8) -> bool {
    e[0] == entry_type && e[4..6] == SVC.to_be_bytes() && e[14..16] == EG.to_be_bytes()
}

/// Whether `d` is a SOME/IP notification for `SVC`/`method`.
fn is_notification(d: &[u8], method: u16) -> bool {
    d.len() >= 16
        && d[0..2] == SVC.to_be_bytes()
        && d[2..4] == method.to_be_bytes()
        && d[14] == 0x02
}

/// The frame peer as a client of the runtime's `SVC`. It sends SD from
/// `PEER_IP:SD_PORT`, numbering its messages from 1, and receives
/// notifications on `PEER_IP:SERVER_PORT`.
struct FrameClient {
    fp: FramePeer,
    session: u16,
    flags: u8,
}

impl FrameClient {
    fn start(flags: u8) -> Self {
        Self {
            fp: FramePeer::start(),
            session: 0,
            flags,
        }
    }

    /// Sends one SD message holding `entries`, all referencing the peer's
    /// endpoint option.
    fn send(&mut self, entries: &[[u8; 16]]) {
        self.session += 1;
        self.fp.send_sd_unicast(
            SocketAddrV4::new(OUR_IP, SD_PORT),
            &build::sd_message_with_flags(self.session, self.flags, entries, &[peer_endpoint()]),
        );
    }

    /// Sends `entries` and waits for a SubscribeEventgroupAck for `EG` with
    /// a TTL other than 0; panics if none arrives within `ACK_WAIT`.
    fn subscribe(&mut self, what: &str, entries: &[[u8; 16]]) {
        self.send(entries);
        let mut other = Vec::new();
        let deadline = Instant::now() + ACK_WAIT;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            let Some((d, _, _)) = self.fp.recv_sd(left, |_| true) else {
                break;
            };
            let acked = parse::sd_entries(&d)
                .iter()
                .any(|(_, e)| is_eventgroup_entry(e, SUBSCRIBE_EVENTGROUP_ACK) && entry_ttl(e) > 0);
            if acked {
                return;
            }
            other.push(parse::sd_entry_types(&d));
        }
        panic!(
            "{}: no SubscribeEventgroupAck for the {what} within {ACK_WAIT:?}; \
             SD entries (type, TTL) received instead: {other:02X?}",
            Rt::NAME
        )
    }

    /// Every notification for `method` that reaches the peer's service port
    /// within `within`.
    fn notifications(&self, method: u16, within: Duration) -> Vec<Vec<u8>> {
        let mut found = Vec::new();
        let deadline = Instant::now() + within;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.fp.recv_unicast(left) {
                Some((d, _)) if is_notification(&d, method) => found.push(d),
                Some(_) => {}
                None => break,
            }
        }
        found
    }

    /// The first notification for `method` within `within`.
    fn notification(&self, method: u16, within: Duration) -> Option<Vec<u8>> {
        let deadline = Instant::now() + within;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.fp.recv_unicast(left) {
                Some((d, _)) if is_notification(&d, method) => return Some(d),
                Some(_) => {}
                None => break,
            }
        }
        None
    }

    /// Waits for the initial value of `FIELD`, then checks that nothing
    /// follows it, so it cannot have been a cyclic publish.
    fn expect_initial_value(&self, after: &str) {
        let d = self.notification(FIELD, INITIAL_WAIT).unwrap_or_else(|| {
            panic!(
                "{}: no notification for 0x{FIELD:04X} within {INITIAL_WAIT:?} of {after}",
                Rt::NAME
            )
        });
        assert_eq!(
            &d[16..],
            FIELD_VALUE,
            "{}: initial value of 0x{FIELD:04X}: {d:02X?}",
            Rt::NAME
        );
        expect_no_repeat(&self.notifications(FIELD, QUIET));
    }
}

/// Fails if a notification for `FIELD` arrived after the initial value with
/// no new trigger; the initial value then cannot be told apart from a
/// cyclic publish.
fn expect_no_repeat(repeats: &[Vec<u8>]) {
    assert!(
        repeats.is_empty(),
        "{}: {} more notifications for 0x{FIELD:04X} within {QUIET:?} of the initial value, \
         with nothing to trigger them: {repeats:02X?}",
        Rt::NAME,
        repeats.len()
    );
}

/// How the frame peer answers a Subscribe when it is the server.
#[derive(Clone, Copy)]
pub(super) enum Answer {
    Ack,
    Nack,
}

/// A Subscribe (TTL other than 0) for `SVC`/`EG` that the frame peer
/// received.
pub(super) struct ReceivedSubscribe {
    at: Instant,
    delivery: Delivery,
    datagram: Vec<u8>,
    /// Where the entry starts in `datagram`.
    offset: usize,
}

impl ReceivedSubscribe {
    fn entry(&self) -> &[u8] {
        &self.datagram[self.offset..self.offset + 16]
    }

    /// Bit 7 of the entry's byte 13.
    fn initial_data_requested(&self) -> bool {
        self.entry()[13] & 0x80 != 0
    }
}

/// The frame peer as the server of `SVC`: it offers every `OFFER_PERIOD`
/// and answers each Subscribe for `EG`. Its multicast and unicast SD
/// messages are numbered separately, each from 1. It records when it sent
/// each Offer and when each Subscribe arrived.
pub(super) struct FrameServer {
    pub(super) fp: FramePeer,
    flags: u8,
    answer: Answer,
    multicast_session: u16,
    unicast_session: u16,
    next_offer: Instant,
    /// When to offer after `next_offer`, relative to it; empty means every
    /// `OFFER_PERIOD`.
    schedule: Vec<Duration>,
    offers: Vec<Instant>,
    subscribes: Vec<ReceivedSubscribe>,
}

impl FrameServer {
    pub(super) fn start(flags: u8, answer: Answer) -> Self {
        Self {
            fp: FramePeer::start(),
            flags,
            answer,
            multicast_session: 0,
            unicast_session: 0,
            next_offer: Instant::now(),
            schedule: Vec::new(),
            offers: Vec::new(),
            subscribes: Vec::new(),
        }
    }

    /// Serves until `done` holds for the Subscribes received so far, or
    /// until `until`; returns whether `done` held.
    fn serve_until(&mut self, until: Instant, done: impl Fn(&[ReceivedSubscribe]) -> bool) -> bool {
        let is_subscribe = |e: &[u8]| is_eventgroup_entry(e, SUBSCRIBE_EVENTGROUP);
        loop {
            if done(&self.subscribes) {
                return true;
            }
            let now = Instant::now();
            if now >= until {
                return false;
            }
            if now >= self.next_offer {
                self.offer();
                self.next_offer = if self.schedule.is_empty() {
                    now + OFFER_PERIOD
                } else {
                    let next = self.schedule.remove(0);
                    self.next_offer + next
                };
            }
            let wait = until.min(self.next_offer).saturating_duration_since(now);
            let Some((d, src, delivery)) = self.fp.recv_sd(wait, |d| {
                parse::sd_entries(d).iter().any(|(_, e)| is_subscribe(e))
            }) else {
                continue;
            };
            let at = Instant::now();
            let mut answers = Vec::new();
            for (offset, e) in parse::sd_entries(&d) {
                // A StopSubscribe gets no answer.
                if !is_subscribe(&e) || entry_ttl(&e) == 0 {
                    continue;
                }
                answers.push(answer_entry(e, self.answer));
                self.subscribes.push(ReceivedSubscribe {
                    at,
                    delivery,
                    datagram: d.clone(),
                    offset,
                });
            }
            if !answers.is_empty() {
                self.unicast_session += 1;
                self.fp.send_sd_unicast(
                    src,
                    &build::sd_message_with_flags(self.unicast_session, self.flags, &answers, &[]),
                );
            }
        }
    }

    fn offer(&mut self) {
        self.multicast_session += 1;
        let offer = build::service_entry(OFFER_SERVICE, 0, 0, 1, 0, SVC, INST, MAJOR, 3, 0);
        self.fp.send_sd_multicast(&build::sd_message_with_flags(
            self.multicast_session,
            self.flags,
            &[offer],
            &[peer_endpoint()],
        ));
        self.offers.push(Instant::now());
    }

    /// Serves until the first Subscribe arrives; panics after `SD_WAIT`.
    pub(super) fn first_subscribe(&mut self) -> &ReceivedSubscribe {
        let found = self.serve_until(Instant::now() + SD_WAIT, |s| !s.is_empty());
        assert!(
            found,
            "{}: no Subscribe for 0x{SVC:04X} eventgroup 0x{EG:04X} within {SD_WAIT:?} of the first Offer",
            Rt::NAME
        );
        &self.subscribes[0]
    }
}

/// The Ack or Nack for the Subscribe entry `e`: the same entry with type
/// 0x07 and no options, and for a Nack a TTL of 0.
fn answer_entry(mut e: [u8; 16], answer: Answer) -> [u8; 16] {
    e[0] = SUBSCRIBE_EVENTGROUP_ACK;
    e[1..4].fill(0);
    if matches!(answer, Answer::Nack) {
        e[9..12].fill(0);
    }
    e
}

scenario!(
    /// PRS_SOMEIPSD_00443, 00449 / feat_req_someipsd_431 — we subscribe to the peer's eventgroup and receive its events.
    p1_client_subscribes_and_receives_events,
    std = run,
    bare_metal = run,
    {
        let mut peer = VsomeipPeer::start();
        peer.send("offer 1234 0001 1 0001 8001 8002 0001");
        let mut rt = Rt::start(client());
        notify_until_event(&mut peer, &mut rt, &[1, 2]);
    }
);

scenario!(
    /// PRS_SOMEIPSD_00462, 00443 — the peer's subscription is acknowledged and receives our events.
    p1_server_accepts_subscription_and_sends_events,
    std = run,
    bare_metal = run,
    {
        let mut peer = VsomeipPeer::start();
        let mut rt = Rt::start(server());
        vsomeip_subscribes(&mut peer);
        rt.publish(EVENT, &[3, 4]);
        peer.expect_where("EVENT", ACK_WAIT, |l| is_peer_event(l, EVENT, &[3, 4]));
    }
);

scenario!(
    /// PRS_SOMEIPSD_00120, 00464 / feat_req_someipsd_691 — a new subscription gets the field's initial value right after the Ack.
    p2_field_initial_value_after_ack,
    std = run,
    bare_metal = run,
    {
        let mut peer = VsomeipPeer::start();
        let _rt = Rt::start(server());
        vsomeip_subscribes(&mut peer);
        peer.expect_where("EVENT", INITIAL_WAIT, |l| {
            is_peer_event(l, FIELD, &FIELD_VALUE)
        });
        // Nothing triggers another notification.
        peer.expect_none("EVENT", QUIET);
    }
);

scenario!(
    /// feat_req_someipsd_1188, 109 — a Subscribe with Initial Data Requested set gets the field's initial value.
    p3_initial_value_on_initial_data_requested,
    std = run,
    bare_metal = run,
    {
        let mut client = FrameClient::start(REBOOT | UNICAST | EXPLICIT_INITIAL_DATA_CONTROL);
        let _rt = Rt::start(server());
        first_offer(&client.fp);
        client.subscribe("Subscribe", &[subscribe_entry(SUBSCRIBE_TTL, true)]);
        client.expect_initial_value("the Ack");
    }
);

scenario!(
    /// PRS_SOMEIPSD_00122 / feat_req_someipsd_833 — a StopSubscribe and a Subscribe in one message trigger the field's initial value again.
    p4_initial_value_on_stop_subscribe_then_subscribe,
    std = run,
    bare_metal = run,
    {
        let mut client = FrameClient::start(REBOOT | UNICAST);
        let _rt = Rt::start(server());
        first_offer(&client.fp);
        client.subscribe("first Subscribe", &[subscribe_entry(SUBSCRIBE_TTL, false)]);
        // Whatever the first subscription brings arrives before the trigger.
        let first = client.notifications(FIELD, QUIET).len();
        client.subscribe(
            "StopSubscribe and Subscribe",
            &[subscribe_entry(0, false), subscribe_entry(SUBSCRIBE_TTL, false)],
        );
        client.expect_initial_value(&format!(
            "the second Ack ({first} notifications for the first subscription)"
        ));
    }
);

scenario!(
    /// PRS_SOMEIPSD_00122, 00120 — vsomeip unsubscribing and subscribing again gets the field's initial value again.
    p4_initial_value_on_stop_subscribe_then_subscribe_vsomeip,
    std = run,
    bare_metal = run,
    {
        // vsomeip may hand its application a field value cached from the
        // first subscription, which this variant cannot tell from a resent
        // initial value. The frame variant above checks the wire.
        let mut peer = VsomeipPeer::start();
        let _rt = Rt::start(server());
        vsomeip_subscribes(&mut peer);
        // Whatever the first subscription brings arrives before the trigger.
        std::thread::sleep(QUIET);
        peer.clear();
        peer.send("unsubscribe 1234 0001 0001");
        peer.send("subscribe 1234 0001 0001 1 8001 8002");
        peer.expect_where("EVENT", ACK_WAIT + INITIAL_WAIT, |l| {
            is_peer_event(l, FIELD, &FIELD_VALUE)
        });
        peer.expect_none("EVENT", QUIET);
    }
);

scenario!(
    /// PRS_SOMEIPSD_00121 / feat_req_someipsd_833 — renewing a valid subscription does not resend the field's initial value.
    p5_renewal_does_not_resend_initial_value,
    std = run,
    bare_metal = run,
    {
        let mut client = FrameClient::start(REBOOT | UNICAST);
        let _rt = Rt::start(server());
        first_offer(&client.fp);
        client.subscribe("first Subscribe", &[subscribe_entry(SUBSCRIBE_TTL, false)]);
        // Control: the initial value is sent at all, so its absence below
        // means something.
        client.notification(FIELD, INITIAL_WAIT).unwrap_or_else(|| {
            panic!(
                "{}: control failed: no initial value for 0x{FIELD:04X} within \
                 {INITIAL_WAIT:?} of the first Ack, so a renewal cannot be checked",
                Rt::NAME
            )
        });
        client.subscribe("renewal", &[subscribe_entry(SUBSCRIBE_TTL, false)]);
        let resent = client.notifications(FIELD, QUIET);
        assert!(
            resent.is_empty(),
            "{}: the renewal of a valid subscription resent 0x{FIELD:04X}: {resent:02X?}",
            Rt::NAME
        );
    }
);

scenario!(
    /// PRS_SOMEIPSD_00388, 00389 — no events reach an endpoint after its StopSubscribe.
    p6_unsubscribe_stops_events,
    std = run,
    bare_metal = run,
    {
        let mut client = FrameClient::start(REBOOT | UNICAST);
        let mut rt = Rt::start(server());
        first_offer(&client.fp);
        client.subscribe("Subscribe", &[subscribe_entry(SUBSCRIBE_TTL, false)]);
        // Control: events flow while the peer is subscribed.
        rt.publish(EVENT, &[6, 1]);
        client.notification(EVENT, ACK_WAIT).unwrap_or_else(|| {
            panic!(
                "{}: control failed: no event within {ACK_WAIT:?} while subscribed",
                Rt::NAME
            )
        });
        client.send(&[subscribe_entry(0, false)]);
        std::thread::sleep(SETTLE);
        for n in 2..5 {
            rt.publish(EVENT, &[6, n]);
            std::thread::sleep(Duration::from_millis(200));
        }
        let after = client.notifications(EVENT, QUIET);
        assert!(
            after.is_empty(),
            "{}: {} events reached the peer after its StopSubscribe: {after:02X?}",
            Rt::NAME,
            after.len()
        );
    }
);

scenario!(
    /// PRS_SOMEIPSD_00388 — our unsubscribe stops the peer's events.
    p6_our_unsubscribe_stops_events,
    std = ignore("the std runtime cannot unsubscribe"),
    bare_metal = ignore("the bare-metal runtime cannot unsubscribe"),
    {
        let mut peer = VsomeipPeer::start();
        peer.send("offer 1234 0001 1 0001 8001 8002 0001");
        let mut rt = Rt::start(client());
        // Control: events flow while we are subscribed.
        notify_until_event(&mut peer, &mut rt, &[6, 1]);
        rt.unsubscribe();
        std::thread::sleep(SETTLE);
        for n in 2..5 {
            peer.send(&format!("notify 1234 0001 8001 06{n:02x}"));
            std::thread::sleep(Duration::from_millis(200));
        }
        rt.expect_none("Event after unsubscribing", QUIET, |o| {
            matches!(o, Observation::Event { event: EVENT, payload, .. } if payload[..] != [6, 1])
        });
    }
);

scenario!(
    /// feat_req_someipsd_1191, 1188 — our first Subscribe to a server that sets Explicit Initial Data Control requests initial data.
    p7_client_requests_initial_values,
    std = run,
    bare_metal = run,
    {
        let mut server =
            FrameServer::start(REBOOT | UNICAST | EXPLICIT_INITIAL_DATA_CONTROL, Answer::Ack);
        let _rt = Rt::start(client());
        let first = server.first_subscribe();
        assert!(
            first.initial_data_requested(),
            "{}: the first Subscribe does not set Initial Data Requested \
             (byte {} of the datagram is {:#04X}): {:02X?}",
            Rt::NAME,
            first.offset + 13,
            first.entry()[13],
            first.datagram
        );
    }
);

scenario!(
    /// PRS_SOMEIPSD_00393, 00394 — the runtime reports a SubscribeEventgroupNack.
    p8_subscribe_reports_nack,
    std = ignore("the std runtime does not report subscription results"),
    bare_metal = ignore("the bare-metal runtime does not report subscription results"),
    {
        let mut server = FrameServer::start(REBOOT | UNICAST, Answer::Nack);
        let mut rt = Rt::start(client());
        server.first_subscribe();
        rt.expect("Subscribed { accepted: false }", ACK_WAIT, |o| {
            matches!(
                o,
                Observation::Subscribed {
                    eventgroup: EG,
                    accepted: false
                }
            )
        });
    }
);

scenario!(
    /// PRS_SOMEIPSD_00446, 00449, 00502 / feat_req_someipsd_431 — we answer each Offer with a Subscribe, which renews the subscription.
    p9_client_renews_subscription,
    std = ignore("the std client leaves subscription renewal to its caller"),
    bare_metal = run,
    {
        let mut server = FrameServer::start(REBOOT | UNICAST, Answer::Ack);
        let _rt = Rt::start(client());
        let first_at = server.first_subscribe().at;
        // Nothing here depends on the Subscribe's TTL.
        let [early, late] = RENEWING_OFFERS;
        server.next_offer = first_at + early;
        server.schedule = vec![late - early];
        server.serve_until(first_at + late + ANSWER_WAIT, |_| false);
        let offers: Vec<Instant> = server
            .offers
            .iter()
            .copied()
            .filter(|&o| o > first_at)
            .take(RENEWING_OFFERS.len())
            .collect();
        assert_eq!(
            offers.len(),
            RENEWING_OFFERS.len(),
            "the frame peer sent too few Offers after the first Subscribe"
        );
        // For each Offer: when it was sent, and how long its answer took.
        let answers: Vec<(Duration, Option<Duration>)> = offers
            .iter()
            .map(|&o| {
                let answer = server
                    .subscribes
                    .iter()
                    .map(|s| s.at.saturating_duration_since(o))
                    .find(|&after| !after.is_zero() && after <= ANSWER_WAIT);
                (o - first_at, answer)
            })
            .collect();
        let subscribes: Vec<Duration> = server
            .subscribes
            .iter()
            .map(|s| s.at.saturating_duration_since(first_at))
            .collect();
        assert!(
            answers.iter().all(|(_, answer)| answer.is_some()),
            "{}: not every Offer after the first Subscribe was answered with a Subscribe \
             within {ANSWER_WAIT:?}; (Offer sent, answered after), relative to the first \
             Subscribe: {answers:.2?}; Subscribes: {subscribes:.2?}",
            Rt::NAME
        );
    }
);

scenario!(
    /// PRS_SOMEIPSD_00501 — we answer the server's Offer with a unicast SD message.
    p10_client_subscribes_by_unicast,
    std = run,
    bare_metal = run,
    {
        let mut server = FrameServer::start(REBOOT | UNICAST, Answer::Ack);
        let _rt = Rt::start(client());
        let first = server.first_subscribe();
        assert!(
            first.delivery == Delivery::Unicast,
            "{}: Subscribe sent to multicast group {SD_GROUP}:{SD_PORT}, not to the server's \
             SD endpoint {PEER_IP}:{SD_PORT}: {:02X?}",
            Rt::NAME,
            first.datagram
        );
    }
);
