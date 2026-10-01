//! Receive-path scenarios: how the runtime under test finds the SOME/IP
//! messages in a UDP datagram.
//!
//! Every scenario first has the frame peer send a well-formed message on its
//! own and waits for the runtime to deliver it. So a scenario that expects
//! nothing to be delivered cannot pass because nothing was received at all.

use std::net::SocketAddrV4;
use std::time::{Duration, Instant};

use crate::Rt;
use crate::interop::consts::*;
use crate::interop::peers::frames::{FramePeer, REBOOT, UNICAST, build};
use crate::interop::runtime::{Observation, Setup, SomeipUnderTest};

use super::discovery::first_offer;
use super::pubsub::{Answer, FrameServer};
use super::rpc::{REQUEST, RESPONSE, replies, request_frame, session};
use super::{CALL_WAIT, QUIET};

/// SOME/IP message types.
const NOTIFICATION: u8 = 0x02;
/// NOTIFICATION with the TP-Flag (0x20) set.
const TP_NOTIFICATION: u8 = 0x22;

/// How soon after the frame peer sends a notification the runtime must
/// report it.
pub(super) const DELIVERY_WAIT: Duration = Duration::from_secs(1);

/// How long the frame peer waits after its Ack before it sends anything
/// else. The Ack can arrive together with one of the frame peer's Offers, and
/// a runtime with a small receive queue may drop a third datagram that
/// follows them at once; the scenarios here measure how one datagram is
/// read, not how many the runtime can queue.
const SETTLE: Duration = Duration::from_millis(200);

/// The payload of each control notification.
const CONTROL: [u8; 1] = [0x10];

/// Where the runtime under test receives notifications.
pub(super) fn client_endpoint() -> SocketAddrV4 {
    SocketAddrV4::new(OUR_IP, CLIENT_PORT)
}

/// A notification for `SVC`/`EVENT` with a Length field that matches
/// `payload`.
pub(super) fn notification(session: u16, payload: &[u8]) -> Vec<u8> {
    let mut d = build::someip_header(SVC, EVENT, 0, session, NOTIFICATION, 0, payload.len());
    d.extend(payload);
    d
}

/// Whether `o` is an event for `SVC`/`EVENT`.
pub(super) fn is_event(o: &Observation) -> bool {
    matches!(
        o,
        Observation::Event {
            service: SVC,
            event: EVENT,
            ..
        }
    )
}

/// Whether `o` is an event for `SVC`/`EVENT` with payload `want`.
fn is_event_with(o: &Observation, want: &[u8]) -> bool {
    matches!(o, Observation::Event { service: SVC, event: EVENT, payload, .. } if payload[..] == *want)
}

/// Starts the runtime with `setup` and the frame peer as the server of
/// `SVC`, and returns once the frame peer has acknowledged the runtime's
/// Subscribe and `SETTLE` has passed. The runtime's notification endpoint is
/// then [`client_endpoint`].
pub(super) fn subscribed(setup: Setup) -> (FrameServer, Rt) {
    let mut server = FrameServer::start(REBOOT | UNICAST, Answer::Ack);
    let rt = Rt::start(setup);
    server.first_subscribe();
    std::thread::sleep(SETTLE);
    (server, rt)
}

/// Sends `CONTROL` in a notification of its own and waits for the runtime to
/// deliver it; panics, naming `then`, if it does not.
fn control(server: &FrameServer, rt: &mut Rt, then: &str) {
    server
        .fp
        .send_unicast(client_endpoint(), &notification(0x0001, &CONTROL));
    let delivered = next_matching(rt, DELIVERY_WAIT, |o| is_event_with(o, &CONTROL));
    if let Err(seen) = delivered {
        panic!(
            "{}: control failed: a well-formed notification with payload {CONTROL:02X?}, sent \
             alone, was not delivered within {DELIVERY_WAIT:?}, so {then} cannot be checked; \
             observed: {seen:?}",
            Rt::NAME
        );
    }
}

/// The payload of every event for `SVC`/`EVENT` the runtime reports within
/// `within`, and everything else it reports, for failure messages.
fn events(rt: &mut Rt, within: Duration) -> (Vec<Vec<u8>>, Vec<String>) {
    let mut events = Vec::new();
    let mut other = Vec::new();
    let deadline = Instant::now() + within;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match rt.next(left) {
            Some(Observation::Event {
                service: SVC,
                event: EVENT,
                payload,
                ..
            }) => events.push(payload),
            Some(o) => other.push(format!("{o:?}")),
            None => break,
        }
    }
    (events, other)
}

/// Waits for an observation matching `f`, as `SomeipUnderTest::expect`
/// does, but returns what it skipped instead of panicking.
pub(super) fn next_matching(
    rt: &mut Rt,
    timeout: Duration,
    f: impl Fn(&Observation) -> bool,
) -> Result<Observation, Vec<String>> {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match rt.next(left) {
            Some(o) if f(&o) => return Ok(o),
            Some(o) => seen.push(format!("{o:?}")),
            None => break,
        }
    }
    Err(seen)
}

scenario!(
    /// PRS_SOMEIP_00535, 00140 / feat_req_someip_702, 319 — every notification in one UDP datagram is delivered.
    x1_all_messages_in_a_datagram_are_delivered,
    std = run,
    bare_metal = run,
    {
        let (server, mut rt) = subscribed(super::pubsub::client());
        // Controls: each notification is delivered when sent alone, so a
        // failure below is caused by sending them together.
        for (session, payload) in [(1, [1]), (2, [2])] {
            server
                .fp
                .send_unicast(client_endpoint(), &notification(session, &payload));
            let delivered = next_matching(&mut rt, DELIVERY_WAIT, |o| is_event_with(o, &payload));
            if let Err(seen) = delivered {
                panic!(
                    "{}: control failed: a notification with payload {payload:02X?}, sent \
                     alone, was not delivered within {DELIVERY_WAIT:?}; observed: {seen:?}",
                    Rt::NAME
                );
            }
        }
        let datagram = [notification(3, &[1]), notification(4, &[2])].concat();
        server.fp.send_unicast(client_endpoint(), &datagram);
        let (events, other) = events(&mut rt, QUIET);
        assert!(
            events == [[1], [2]],
            "{}: one datagram holding two notifications, with payloads [01] and [02], was \
             delivered as {} events with payloads {events:02X?}; want both, in order. Each was \
             delivered when sent alone. Other observations: {other:?}; datagram: {datagram:02X?}",
            Rt::NAME,
            events.len()
        );
    }
);

scenario!(
    /// PRS_SOMEIP_00535, 00140 / feat_req_someip_702, 319 — every request in one UDP datagram is answered.
    x1_all_requests_in_a_datagram_are_answered,
    std = run,
    bare_metal = run,
    {
        let fp = FramePeer::start();
        let _rt = Rt::start(super::rpc::server());
        first_offer(&fp);
        let to = SocketAddrV4::new(OUR_IP, SERVER_PORT);
        let answers = |d: &[u8], s: u16| d[14] == RESPONSE && session(d) == s;
        // Controls: each REQUEST is answered when sent alone, so a failure
        // below is caused by sending them together.
        for (s, payload) in [(1, [1]), (2, [2])] {
            fp.send_unicast(to, &request_frame(METHOD, REQUEST, s, &payload));
            let answered = replies(&fp, METHOD, CALL_WAIT, |d| answers(d, s))
                .iter()
                .any(|d| answers(d, s));
            assert!(
                answered,
                "{}: control failed: no RESPONSE within {CALL_WAIT:?} to a REQUEST for \
                 0x{METHOD:04X} with session ID {s:#06X} and payload {payload:02X?}, sent alone",
                Rt::NAME
            );
        }
        let datagram = [
            request_frame(METHOD, REQUEST, 3, &[1]),
            request_frame(METHOD, REQUEST, 4, &[2]),
        ]
        .concat();
        fp.send_unicast(to, &datagram);
        let got = replies(&fp, METHOD, CALL_WAIT, |_| false);
        let answered: Vec<(u16, Vec<u8>)> = got
            .iter()
            .filter(|d| d[14] == RESPONSE)
            .map(|d| (session(d), d[16..].to_vec()))
            .collect();
        assert!(
            answered.contains(&(3, vec![1])) && answered.contains(&(4, vec![2])),
            "{}: one datagram holding two REQUESTs, with session IDs 0x0003 and 0x0004, got \
             RESPONSEs (session ID, payload) {answered:02X?} within {CALL_WAIT:?}; want one \
             for each. Each was answered when sent alone. All replies: {got:02X?}",
            Rt::NAME
        );
    }
);

scenario!(
    /// PRS_SOMEIP_00140, 00910 / feat_req_someip_77, 319 — a message ends where its Length field says; trailing bytes are not its payload.
    ///
    /// The 3 bytes after the message are too short to be another message,
    /// which takes at least 16 (PRS_SOMEIP_00910), so they are malformed and
    /// must not be delivered.
    x2_length_field_ends_the_message,
    std = run,
    bare_metal = run,
    {
        let (server, mut rt) = subscribed(super::pubsub::client());
        control(&server, &mut rt, "the Length field");
        let datagram = [notification(2, &[1]), vec![0xEE; 3]].concat();
        server.fp.send_unicast(client_endpoint(), &datagram);
        let (events, other) = events(&mut rt, QUIET);
        assert!(
            events == [[1]],
            "{}: a notification whose Length field covers payload [01], followed by 3 more \
             bytes in the datagram, was delivered as {} events with payloads {events:02X?}; \
             want exactly one, with payload [01]. Other observations: {other:?}; datagram: \
             {datagram:02X?}",
            Rt::NAME,
            events.len()
        );
    }
);

scenario!(
    /// PRS_SOMEIP_00910 / feat_req_someip_77 — a message whose Length field runs past the end of the datagram is malformed and not delivered.
    x2_length_past_the_datagram_is_malformed,
    std = run,
    bare_metal = run,
    {
        let (server, mut rt) = subscribed(super::pubsub::client());
        control(&server, &mut rt, "a malformed Length field");
        // The Length field says 10 bytes of payload; only 4 follow.
        let mut datagram = build::someip_header(SVC, EVENT, 0, 2, NOTIFICATION, 0, 10);
        datagram.extend([0x20, 0x21, 0x22, 0x23]);
        server.fp.send_unicast(client_endpoint(), &datagram);
        rt.expect_none(
            "Event for a notification whose Length field says 10 bytes of payload but whose \
             datagram holds only 4",
            QUIET,
            is_event,
        );
    }
);

scenario!(
    /// PRS_SOMEIP_00367, 00722 / feat_req_someip_761, feat_req_someiptp_765 — a SOME/IP-TP segment is not delivered as a whole message.
    ///
    /// The segment is the first of several (More Segments set), so neither a
    /// runtime that reassembles SOME/IP-TP nor one that does not may deliver
    /// anything for it.
    x3_tp_segments_are_not_delivered_as_messages,
    std = run,
    bare_metal = run,
    {
        let (server, mut rt) = subscribed(super::pubsub::client());
        control(&server, &mut rt, "a SOME/IP-TP segment");
        // TP header: offset 0, More Segments set; then 16 bytes of segment.
        let segment = [[0x00, 0x00, 0x00, 0x01].as_slice(), &[0x30; 16]].concat();
        let mut datagram =
            build::someip_header(SVC, EVENT, 0, 2, TP_NOTIFICATION, 0, segment.len());
        datagram.extend(&segment);
        server.fp.send_unicast(client_endpoint(), &datagram);
        rt.expect_none(
            "Event for the first SOME/IP-TP segment of a notification (message type 0x22)",
            QUIET,
            is_event,
        );
    }
);
