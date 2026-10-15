//! Request/response scenarios.
//!
//! Every client scenario waits until the runtime has found the peer's
//! service, then checks something only the peer can produce: the peer's
//! record of the request, or a reply only the peer sends. So a call that
//! returns without reaching the peer cannot pass.

use std::any::Any;
use std::net::SocketAddrV4;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::Rt;
use crate::interop::consts::*;
use crate::interop::peers::frames::{FramePeer, OFFER_SERVICE, REBOOT, UDP, UNICAST, build};
use crate::interop::peers::vsomeip::{PeerLine, VsomeipPeer};
use crate::interop::runtime::{CallOutcome, Consume, Observation, Offer, Setup, SomeipUnderTest};

use super::discovery::first_offer;
use super::{CALL_WAIT, QUIET, SD_WAIT};

/// SOME/IP message types.
pub(super) const REQUEST: u8 = 0x00;
const REQUEST_NO_RETURN: u8 = 0x01;
pub(super) const RESPONSE: u8 = 0x80;

/// Return codes.
const E_NOT_OK: u8 = 0x01;
const E_UNKNOWN_METHOD: u8 = 0x03;

/// A method that `SVC` does not have.
const UNKNOWN_METHOD: u16 = 0x0099;
/// `SVC`'s fire-and-forget method. A REQUEST_NO_RETURN for a method
/// defined for REQUEST is discarded, so R4 needs a method of its own.
const FIRE_METHOD: u16 = 0x0002;

/// The client ID in the frame peer's requests.
const PEER_CLIENT: u16 = 0x0042;

/// The vsomeip command that offers `SVC` with `METHOD`.
const VSOMEIP_OFFER: &str = "offer 1234 0001 1 0001 8001 8002 0001";

/// How often the frame peer offers when it is the server.
const OFFER_PERIOD: Duration = Duration::from_secs(1);
/// How long the frame peer's server thread waits for a request at a time.
const POLL: Duration = Duration::from_millis(10);

/// R5: how many calls go unanswered (more than 64), and how long each waits.
const UNANSWERED: usize = 70;
const SHORT_CALL: Duration = Duration::from_millis(50);

/// The runtime as the server of `SVC`, with `METHOD` and `FIRE_METHOD`.
pub(super) fn server() -> Setup {
    Setup {
        offer: Some(Offer {
            service: SVC,
            ttl_s: 3,
            events: vec![EVENT],
            fields: vec![],
            methods: vec![METHOD, FIRE_METHOD],
        }),
        consume: None,
    }
}

/// The runtime as a client of `SVC`.
///
/// The bare-metal runtime cannot start without an offer, so it also offers
/// an unrelated service. Its client scenarios are ignored; the offer lets
/// them reach the missing call API when they are run anyway.
fn client() -> Setup {
    let unrelated = Offer {
        service: 0x5678,
        ttl_s: 3,
        events: vec![],
        fields: vec![],
        methods: vec![],
    };
    Setup {
        offer: cfg!(feature = "bare-metal-runtime").then_some(unrelated),
        consume: Some(Consume {
            subscribe: false,
            e2e: None,
        }),
    }
}

/// Waits until the runtime has found `SVC`, so its calls can reach it.
///
/// The bare-metal runtime reports no discovered services and makes no
/// calls, so on it this returns at once and the call that follows names
/// that gap.
fn await_service(rt: &mut Rt) {
    if cfg!(feature = "bare-metal-runtime") {
        return;
    }
    rt.expect("ServiceAvailable for 0x1234", SD_WAIT, |o| {
        matches!(
            o,
            Observation::ServiceAvailable {
                service: SVC,
                instance: INST,
                ..
            }
        )
    });
}

/// vsomeip looks for the runtime's `SVC`; panics unless it finds it.
fn vsomeip_finds_us(peer: &mut VsomeipPeer) {
    peer.send("require 1234 0001 1");
    peer.expect_where("AVAILABLE", SD_WAIT, |l| l.hex("service") == u32::from(SVC));
}

fn field<'a>(l: &'a PeerLine, key: &str) -> &'a str {
    l.fields.get(key).map_or("", String::as_str)
}

fn is_peer_request(l: &PeerLine, method: u16, payload: &[u8]) -> bool {
    l.hex("method") == u32::from(method) && l.payload() == payload
}

fn method(d: &[u8]) -> u16 {
    u16::from_be_bytes([d[2], d[3]])
}

pub(super) fn session(d: &[u8]) -> u16 {
    u16::from_be_bytes([d[10], d[11]])
}

/// Whether `d` is a SOME/IP message for `SVC`/`method`.
fn is_for(d: &[u8], method: u16) -> bool {
    d.len() >= 16 && d[0..2] == SVC.to_be_bytes() && d[2..4] == method.to_be_bytes()
}

/// A request for `SVC`/`method` from the frame peer.
pub(super) fn request_frame(method: u16, msg_type: u8, session: u16, payload: &[u8]) -> Vec<u8> {
    let mut d = build::someip_header(
        SVC,
        method,
        PEER_CLIENT,
        session,
        msg_type,
        0,
        payload.len(),
    );
    d.extend(payload);
    d
}

/// The RESPONSE to the request `d`: its message ID and request ID, return
/// code E_OK, and its payload echoed.
fn response_to(d: &[u8]) -> Vec<u8> {
    let payload = &d[16..];
    let mut r = build::someip_header(
        u16::from_be_bytes([d[0], d[1]]),
        method(d),
        u16::from_be_bytes([d[8], d[9]]),
        session(d),
        RESPONSE,
        0,
        payload.len(),
    );
    r.extend(payload);
    r
}

/// Every datagram for `SVC`/`method` that reaches the frame peer's service
/// port within `within`, stopping early after one for which `last` holds.
pub(super) fn replies(
    fp: &FramePeer,
    method: u16,
    within: Duration,
    last: impl Fn(&[u8]) -> bool,
) -> Vec<Vec<u8>> {
    let mut found = Vec::new();
    let deadline = Instant::now() + within;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match fp.recv_unicast(left) {
            Some((d, _)) if is_for(&d, method) => {
                let done = last(&d);
                found.push(d);
                if done {
                    break;
                }
            }
            Some(_) => {}
            None => break,
        }
    }
    found
}

/// `rt.call` for `METHOD`, with a panic in it returned as its message. The
/// std adapter reports a local error from the library by panicking.
fn call_catching(rt: &mut Rt, payload: &[u8], timeout: Duration) -> Result<CallOutcome, String> {
    catch_unwind(AssertUnwindSafe(|| rt.call(METHOD, payload, timeout)))
        .map_err(|e| panic_message(&*e))
}

fn panic_message(e: &(dyn Any + Send)) -> String {
    e.downcast_ref::<String>()
        .cloned()
        .or_else(|| e.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_else(|| "a panic without a message".to_owned())
}

/// The frame peer as the server of `SVC`, on a thread of its own so it can
/// answer while a call blocks the test thread. It offers `SVC` every
/// `OFFER_PERIOD` and keeps every message for `SVC` that reaches its
/// service port. It drops every request until [`answer`](Self::answer) is
/// called, and from then on answers each REQUEST with its payload echoed.
struct FrameMethodServer {
    answering: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    thread: Option<JoinHandle<()>>,
}

impl FrameMethodServer {
    fn start() -> Self {
        let fp = FramePeer::start();
        let answering = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let thread = std::thread::Builder::new()
            .name("frame-method-server".into())
            .spawn({
                let (answering, stop, requests) =
                    (answering.clone(), stop.clone(), requests.clone());
                move || serve(&fp, &answering, &stop, &requests)
            })
            .expect("could not spawn the frame peer's server thread");
        Self {
            answering,
            stop,
            requests,
            thread: Some(thread),
        }
    }

    fn answer(&self) {
        self.answering.store(true, Ordering::Release);
    }

    fn requests(&self) -> Vec<Vec<u8>> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The first request kept for which `pred` holds, waiting up to `timeout`.
    fn wait_for_request(&self, timeout: Duration, pred: impl Fn(&[u8]) -> bool) -> Option<Vec<u8>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(d) = self.requests().into_iter().find(|d| pred(d)) {
                return Some(d);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(POLL);
        }
    }
}

impl Drop for FrameMethodServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(
    fp: &FramePeer,
    answering: &AtomicBool,
    stop: &AtomicBool,
    requests: &Mutex<Vec<Vec<u8>>>,
) {
    let offer = build::service_entry(OFFER_SERVICE, 0, 0, 1, 0, SVC, INST, MAJOR, 3, 0);
    let options = [build::ipv4_endpoint_option(PEER_IP, UDP, SERVER_PORT)];
    let mut sd_session = 0u16;
    let mut next_offer = Instant::now();
    while !stop.load(Ordering::Acquire) {
        let now = Instant::now();
        if now >= next_offer {
            sd_session += 1;
            fp.send_sd_multicast(&build::sd_message_with_flags(
                sd_session,
                REBOOT | UNICAST,
                &[offer],
                &options,
            ));
            next_offer = now + OFFER_PERIOD;
        }
        let wait = next_offer.saturating_duration_since(now).min(POLL);
        let Some((d, src)) = fp.recv_unicast(wait) else {
            continue;
        };
        if d.len() < 16 || d[0..2] != SVC.to_be_bytes() {
            continue;
        }
        let reply =
            (answering.load(Ordering::Acquire) && d[14] == REQUEST).then(|| response_to(&d));
        requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(d);
        if let Some(reply) = reply {
            fp.send_unicast(src, &reply);
        }
    }
}

scenario!(
    /// PRS_SOMEIP_00920, 00922 / feat_req_someip_329, 338 — our call to the peer's method gets the peer's response.
    r1_client_call_gets_response,
    std = run,
    bare_metal = ignore("the bare-metal runtime has no client-side method calls (#187)"),
    {
        let mut peer = VsomeipPeer::start();
        peer.send(VSOMEIP_OFFER);
        let mut rt = Rt::start(client());
        await_service(&mut rt);
        let outcome = rt.call(METHOD, &[9], CALL_WAIT);
        let request = peer.expect_where("REQUEST", CALL_WAIT, |l| is_peer_request(l, METHOD, &[9]));
        assert_eq!(
            field(&request, "type"),
            "request",
            "{}: message type of our call: {}",
            Rt::NAME,
            request.raw
        );
        // vsomeip echoes the payload, so only its response carries [09].
        assert!(
            matches!(
                &outcome,
                CallOutcome::Response { return_code: None | Some(0), payload } if payload[..] == [9]
            ),
            "{}: call of 0x{METHOD:04X} with payload [09]: got {outcome:?}, want a Response \
             with return code E_OK and payload [09]",
            Rt::NAME
        );
    }
);

scenario!(
    /// PRS_SOMEIP_00922 / feat_req_someip_338 — the peer's call to our method gets a RESPONSE with E_OK.
    r1_server_answers_call,
    std = run,
    bare_metal = run,
    {
        let mut peer = VsomeipPeer::start();
        let _rt = Rt::start(server());
        vsomeip_finds_us(&mut peer);
        peer.clear();
        peer.send("call 1234 0001 0001 09");
        let response = peer.expect_where("RESPONSE", CALL_WAIT, |l| {
            l.hex("method") == u32::from(METHOD)
        });
        assert!(
            field(&response, "type") == "response"
                && response.hex("return_code") == 0
                && response.payload() == [9],
            "{}: our answer to vsomeip's call: {}; want type=response return_code=0x00 payload=09",
            Rt::NAME,
            response.raw
        );
    }
);

scenario!(
    /// PRS_SOMEIP_00922, 00191 / feat_req_someip_338, 371 — the return code of the peer's ERROR reaches our caller.
    r2_error_return_code_reaches_caller,
    std = ignore("an ERROR reply's return code must reach the caller (#183)"),
    bare_metal = ignore("the bare-metal runtime has no client-side method calls (#187)"),
    {
        let mut peer = VsomeipPeer::start();
        peer.send(VSOMEIP_OFFER);
        // `send` returns only once vsomeip has acknowledged the command with
        // `OK error-reply`.
        peer.send("error-reply 0001 01");
        let mut rt = Rt::start(client());
        await_service(&mut rt);
        let outcome = rt.call(METHOD, &[2], CALL_WAIT);
        // Control: the call reached vsomeip, which answers it with an ERROR.
        peer.expect_where("REQUEST", CALL_WAIT, |l| is_peer_request(l, METHOD, &[2]));
        assert!(
            matches!(
                outcome,
                CallOutcome::Error {
                    return_code: E_NOT_OK
                } | CallOutcome::Response {
                    return_code: Some(E_NOT_OK),
                    ..
                }
            ),
            "{}: vsomeip answered our call with an ERROR carrying E_NOT_OK (0x01), \
             but the call returned {outcome:?}",
            Rt::NAME
        );
    }
);

scenario!(
    /// PRS_SOMEIP_00195 (Figure 5.12), 00701, 00190 / feat_req_someip_141, 816 — a call of a method we do not have gets return code E_UNKNOWN_METHOD.
    ///
    /// Discarding the request is also permitted by both specifications; this
    /// stack answers with E_UNKNOWN_METHOD, in a RESPONSE or an ERROR message.
    r3_unknown_method_gets_error,
    std = ignore("a call of an unknown method must get E_UNKNOWN_METHOD, not a success (#182)"),
    bare_metal = ignore("a call of an unknown method must get E_UNKNOWN_METHOD, not a success (#182)"),
    {
        let mut peer = VsomeipPeer::start();
        let _rt = Rt::start(server());
        vsomeip_finds_us(&mut peer);
        // Control: a call of the method we have succeeds, so the server is up,
        // and it does not answer every method with an error.
        peer.send("call 1234 0001 0001 03");
        let control =
            peer.next_where("RESPONSE", CALL_WAIT, |l| l.hex("method") == u32::from(METHOD));
        let succeeded = control
            .as_ref()
            .is_some_and(|l| field(l, "type") == "response" && l.hex("return_code") == 0);
        assert!(
            succeeded,
            "{}: control failed: a call of 0x{METHOD:04X}, which we offer, got {}; want \
             type=response return_code=0x00, so the unknown method cannot be checked",
            Rt::NAME,
            control.map_or(format!("no reply within {CALL_WAIT:?}"), |l| l.raw)
        );
        peer.clear();
        peer.send("call 1234 0001 0099 03");
        let reply = peer.next_where("RESPONSE", CALL_WAIT, |l| {
            l.hex("method") == u32::from(UNKNOWN_METHOD)
        });
        let want = "want return_code=0x03 (E_UNKNOWN_METHOD) in a RESPONSE or an ERROR message";
        let Some(reply) = reply else {
            panic!(
                "{}: no reply within {CALL_WAIT:?} to a call of 0x{UNKNOWN_METHOD:04X}, \
                 a method 0x{SVC:04X} does not have; {want}",
                Rt::NAME
            )
        };
        let return_code = reply.hex("return_code");
        let verdict = if return_code == u32::from(E_UNKNOWN_METHOD) {
            // An ERROR message carries no payload; a RESPONSE may.
            (field(&reply, "type") == "error" && !reply.payload().is_empty())
                .then(|| "answered with an ERROR message that carries a payload".to_owned())
        } else if return_code == 0 {
            Some("answered as a success".to_owned())
        } else {
            Some(format!("answered with return code {return_code:#04x}"))
        };
        if let Some(verdict) = verdict {
            panic!(
                "{}: a call of 0x{UNKNOWN_METHOD:04X}, a method 0x{SVC:04X} does not have, was \
                 {verdict}: {}; {want}",
                Rt::NAME,
                reply.raw
            );
        }
    }
);

scenario!(
    /// PRS_SOMEIP_00939, 00924 — our fire-and-forget request is a REQUEST_NO_RETURN with session ID 0x0000 (vsomeip's view).
    r4_fire_and_forget_uses_session_zero,
    std = ignore("a fire-and-forget request must use session ID 0x0000 (#185)"),
    bare_metal = ignore("the bare-metal runtime has no client-side method calls (#187)"),
    {
        let mut peer = VsomeipPeer::start();
        peer.send(VSOMEIP_OFFER);
        let mut rt = Rt::start(client());
        await_service(&mut rt);
        peer.clear();
        rt.fire_and_forget(METHOD, &[7]);
        let request = peer.expect_where("REQUEST", CALL_WAIT, |l| is_peer_request(l, METHOD, &[7]));
        let mut problems = Vec::new();
        if field(&request, "type") != "request_no_return" {
            problems.push(format!(
                "message type: got {}, want request_no_return",
                field(&request, "type")
            ));
        }
        if request.hex("session") != 0 {
            problems.push(format!(
                "session ID: got {}, want 0x0000",
                field(&request, "session")
            ));
        }
        assert!(
            problems.is_empty(),
            "{}: our fire-and-forget request, as vsomeip received it:\n  {}\n{}",
            Rt::NAME,
            problems.join("\n  "),
            request.raw
        );
    }
);

scenario!(
    /// PRS_SOMEIP_00939, 00924 — our fire-and-forget request is a REQUEST_NO_RETURN with session ID 0x0000 and return code E_OK (wire bytes).
    r4_fire_and_forget_uses_session_zero_frame,
    std = ignore("a fire-and-forget request must use session ID 0x0000 (#185)"),
    bare_metal = ignore("the bare-metal runtime has no client-side method calls (#187)"),
    {
        let server = FrameMethodServer::start();
        let mut rt = Rt::start(client());
        await_service(&mut rt);
        rt.fire_and_forget(METHOD, &[7]);
        let d = server
            .wait_for_request(CALL_WAIT, |d| is_for(d, METHOD) && d[16..] == [7])
            .unwrap_or_else(|| {
                panic!(
                    "{}: no request for 0x{METHOD:04X} with payload [07] reached the peer within \
                     {CALL_WAIT:?}; it received: {:02X?}",
                    Rt::NAME,
                    server.requests()
                )
            });
        let mut problems = Vec::new();
        if d[14] != REQUEST_NO_RETURN {
            problems.push(format!(
                "message type (byte 14): got {:#04X}, want 0x01",
                d[14]
            ));
        }
        if session(&d) != 0 {
            problems.push(format!(
                "session ID (bytes 10-11): got {:#06X}, want 0x0000",
                session(&d)
            ));
        }
        if d[15] != 0 {
            problems.push(format!(
                "return code (byte 15): got {:#04X}, want 0x00",
                d[15]
            ));
        }
        assert!(
            problems.is_empty(),
            "{}: our fire-and-forget request:\n  {}\n{d:02X?}",
            Rt::NAME,
            problems.join("\n  ")
        );
    }
);

scenario!(
    /// PRS_SOMEIP_00189, 00537, 00385 / feat_req_someip_345, 348 — we send no reply to a REQUEST_NO_RETURN for our fire-and-forget method.
    r4_no_reply_to_fire_and_forget,
    std = ignore("a REQUEST_NO_RETURN must get no reply (#182)"),
    bare_metal = ignore("a REQUEST_NO_RETURN must get no reply (#182)"),
    {
        let fp = FramePeer::start();
        let mut rt = Rt::start(server());
        first_offer(&fp);
        let to = SocketAddrV4::new(OUR_IP, SERVER_PORT);
        // Control: a REQUEST is answered, so the server is up and replying.
        let control = request_frame(METHOD, REQUEST, 0x0001, &[1]);
        fp.send_unicast(to, &control);
        let answers = |d: &[u8]| d[14] == RESPONSE && d[8..12] == control[8..12];
        let answered = replies(&fp, METHOD, CALL_WAIT, answers)
            .iter()
            .any(|d| answers(d));
        assert!(
            answered,
            "{}: control failed: no RESPONSE within {CALL_WAIT:?} to a REQUEST for \
             0x{METHOD:04X}, so silence after a REQUEST_NO_RETURN would mean nothing",
            Rt::NAME
        );
        fp.send_unicast(
            to,
            &request_frame(FIRE_METHOD, REQUEST_NO_RETURN, 0x0000, &[7]),
        );
        let sent = replies(&fp, FIRE_METHOD, QUIET, |_| false);
        let types: Vec<u8> = sent.iter().map(|d| d[14]).collect();
        assert!(
            sent.is_empty(),
            "{}: we replied to a REQUEST_NO_RETURN for 0x{FIRE_METHOD:04X} within {QUIET:?} \
             ({} datagrams, message types {types:02X?}): {sent:02X?}",
            Rt::NAME,
            sent.len()
        );
        // Control: the request reached the server's application, so its
        // silence was not a lost datagram.
        rt.expect("the REQUEST_NO_RETURN as a Request", CALL_WAIT, |o| {
            matches!(
                o,
                Observation::Request { method: FIRE_METHOD, payload, .. } if payload[..] == [7]
            )
        });
    }
);

scenario!(
    /// PRS_SOMEIP §5.2.6.3.1 (Figure 5.13) / feat_req_someip_80, 436 — calls that time out unanswered do not stop later calls from getting responses.
    r5_unanswered_requests_do_not_stall_client,
    std = ignore("calls that time out must not stop later calls from getting responses (#184)"),
    bare_metal = ignore("the bare-metal runtime has no client-side method calls (#187)"),
    {
        let server = FrameMethodServer::start();
        let mut rt = Rt::start(client());
        await_service(&mut rt);
        let outcomes: Vec<Result<CallOutcome, String>> = (0..UNANSWERED)
            .map(|n| call_catching(&mut rt, &[n as u8], SHORT_CALL))
            .collect();
        let unanswered = server.requests().len();
        server.answer();
        let last = call_catching(&mut rt, &[0xA5], CALL_WAIT);
        let last_reached = server
            .requests()
            .iter()
            .any(|d| d[14] == REQUEST && d[16..] == [0xA5]);

        let describe = |r: &Result<CallOutcome, String>| match r {
            Ok(o) => format!("returned {o:?}"),
            Err(panic) => format!("failed: {panic}"),
        };
        let mut problems = Vec::new();
        let failed: Vec<usize> = outcomes
            .iter()
            .enumerate()
            .filter(|(_, r)| !matches!(r, Ok(CallOutcome::NoReply)))
            .map(|(i, _)| i + 1)
            .collect();
        if let Some(&first) = failed.first() {
            problems.push(format!(
                "{} of the {UNANSWERED} unanswered calls did not time out (calls {:?}); \
                 call {first} {}",
                failed.len(),
                failed,
                describe(&outcomes[first - 1])
            ));
        }
        // Lower bound: more than 64 requests went out unanswered.
        if unanswered <= 64 {
            problems.push(format!(
                "the peer received only {unanswered} requests before it started answering; \
                 want more than 64"
            ));
        }
        if !matches!(&last, Ok(CallOutcome::Response { payload, .. }) if payload[..] == [0xA5]) {
            problems.push(format!(
                "the call after the peer started answering {}; want a Response with payload \
                 [A5] (its request {})",
                describe(&last),
                if last_reached {
                    "reached the peer, which answered it"
                } else {
                    "never reached the peer"
                }
            ));
        }
        assert!(
            problems.is_empty(),
            "{}: {UNANSWERED} calls with a {SHORT_CALL:?} timeout each; the peer received \
             {unanswered} requests and answered none of them:\n  {}",
            Rt::NAME,
            problems.join("\n  ")
        );
    }
);
