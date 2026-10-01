//! E2E scenarios: a Profile 5 protected event, checked by the runtime under
//! test.
//!
//! The protected frames are pinned as bytes. A test below recomputes their
//! CRCs with a CRC-16/CCITT-FALSE written from the specification, which
//! reproduces AUTOSAR's Profile 5 examples, so the frames do not rest on
//! simple-someip's own E2E code. On the std target the scenario builds them
//! with that code, and a second test checks that it produces the pinned
//! bytes.

use std::time::{Duration, Instant};

use crate::Rt;
use crate::interop::consts::*;
use crate::interop::peers::vsomeip::VsomeipPeer;
use crate::interop::runtime::{Consume, E2eSpec, Observation, Setup, SomeipUnderTest};

use super::receive_path::{DELIVERY_WAIT, client_endpoint, is_event, next_matching, subscribed};
use super::{QUIET, SD_WAIT};

/// The Data ID and maximum counter delta of the protected event.
const DATA_ID: u16 = 0x9C6E;
const MAX_DELTA: u8 = 2;

/// The event's payload, before protection.
const PAYLOAD: [u8; 5] = [0x01, 0x02, 0x03, 0x04, 0x05];

/// Where the E2E header starts: after the 16-byte SOME/IP header, which is
/// offset 64 in the protected data (header bytes 8–15 come first).
const E2E_AT: usize = 16;

/// The notification for `SVC`/`EVENT` with session ID 0x0001 and `PAYLOAD`
/// protected with Profile 5 at offset 64, counter 0, Data ID 0x9C6E.
const PROTECTED_0: [u8; 24] = [
    0x12, 0x34, 0x80, 0x01, // message ID
    0x00, 0x00, 0x00, 0x10, // length: 8 + 3 + 5
    0x00, 0x00, 0x00, 0x01, // client ID 0, session ID 1
    0x01, 0x01, 0x02, 0x00, // versions, NOTIFICATION, E_OK
    0x3D, 0xBC, 0x00, // CRC 0xBC3D (little-endian), counter 0
    0x01, 0x02, 0x03, 0x04, 0x05,
];

/// The next notification: session ID 0x0002, counter 1.
const PROTECTED_1: [u8; 24] = [
    0x12, 0x34, 0x80, 0x01, // message ID
    0x00, 0x00, 0x00, 0x10, // length: 8 + 3 + 5
    0x00, 0x00, 0x00, 0x02, // client ID 0, session ID 2
    0x01, 0x01, 0x02, 0x00, // versions, NOTIFICATION, E_OK
    0x4B, 0x34, 0x01, // CRC 0x344B (little-endian), counter 1
    0x01, 0x02, 0x03, 0x04, 0x05,
];

/// The vsomeip command that offers `SVC` with `EVENT` in `EG`.
const VSOMEIP_OFFER: &str = "offer 1234 0001 1 0001 8001 8002 0001";

/// The runtime as a client of `SVC`, subscribing to `EG` and checking
/// `EVENT` with Profile 5.
fn e2e_client() -> Setup {
    let mut setup = super::pubsub::client();
    setup.consume = Some(Consume {
        subscribe: true,
        e2e: Some(E2eSpec {
            data_id: DATA_ID,
            data_length_bits: (PAYLOAD.len() * 8) as u16,
            max_delta: MAX_DELTA,
        }),
    });
    setup
}

/// The two protected notifications, counters 0 and 1.
#[cfg(not(feature = "bare-metal-runtime"))]
fn protected_frames() -> [Vec<u8>; 2] {
    use simple_someip::e2e::{Profile5Config, Profile5State, protect_profile5_with_header};

    use crate::interop::peers::frames::build;

    let config = Profile5Config::new(DATA_ID, PAYLOAD.len() as u16, MAX_DELTA);
    let mut state = Profile5State::new();
    [1, 2].map(|session| {
        let mut d = build::someip_header(SVC, EVENT, 0, session, 0x02, 0, 3 + PAYLOAD.len());
        let upper: [u8; 8] = d[8..16].try_into().expect("an 8-byte upper header");
        let mut protected = [0; 3 + PAYLOAD.len()];
        let n = protect_profile5_with_header(&config, &mut state, &PAYLOAD, upper, &mut protected)
            .expect("the buffer fits the protected payload");
        d.extend(&protected[..n]);
        d
    })
}

/// The two protected notifications, counters 0 and 1. The bare-metal target
/// uses the pinned bytes.
#[cfg(feature = "bare-metal-runtime")]
fn protected_frames() -> [Vec<u8>; 2] {
    [PROTECTED_0.to_vec(), PROTECTED_1.to_vec()]
}

/// CRC-16/CCITT-FALSE (polynomial 0x1021, initial value 0xFFFF, no
/// reflection, no final XOR): the CRC that PRS_E2E_00400 specifies for
/// Profile 5.
fn crc16_ccitt_false(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// The Profile 5 CRC of `data` with its E2E header at byte `at`: over every
/// byte except the two CRC bytes, then the Data ID, low byte first
/// (PRS_E2E_00399, 00401).
fn profile5_crc(data: &[u8], at: usize, data_id: u16) -> u16 {
    let input = [&data[..at], &data[at + 2..], &data_id.to_le_bytes()].concat();
    crc16_ccitt_false(&input)
}

#[test]
fn crc_reproduces_autosar_profile5_examples() {
    // The CRC catalogue's check value.
    assert_eq!(crc16_ccitt_false(b"123456789"), 0x29B1);
    // Table 6.27: Data ID 0x1234, offset 0, counter 0, 5 zero data bytes.
    let short = [0x1C, 0xCA, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    assert_eq!(profile5_crc(&short, 0, 0x1234).to_le_bytes(), [0x1C, 0xCA]);
    // Table 6.28: the same at offset 64, after 8 bytes of upper header.
    let mut someip = [0u8; 16];
    someip[8..10].copy_from_slice(&[0x28, 0x91]);
    assert_eq!(profile5_crc(&someip, 8, 0x1234).to_le_bytes(), [0x28, 0x91]);
}

#[test]
fn pinned_frames_carry_the_profile5_crc() {
    for (counter, frame) in [PROTECTED_0, PROTECTED_1].iter().enumerate() {
        // The protected data starts at the Request ID, header byte 8.
        let crc = profile5_crc(&frame[8..], E2E_AT - 8, DATA_ID);
        assert_eq!(
            frame[E2E_AT..E2E_AT + 2],
            crc.to_le_bytes(),
            "frame with counter {counter}"
        );
        assert_eq!(usize::from(frame[E2E_AT + 2]), counter);
        assert_eq!(frame[E2E_AT + 3..], PAYLOAD);
    }
}

#[cfg(not(feature = "bare-metal-runtime"))]
#[test]
fn library_protection_matches_the_pinned_frames() {
    let [first, second] = protected_frames();
    assert_eq!(first, PROTECTED_0, "counter 0: {first:02X?}");
    assert_eq!(second, PROTECTED_1, "counter 1: {second:02X?}");
}

scenario!(
    /// PRS_E2E_00399–00401 (Tables 6.27, 6.28) / feat_req_someip_102 — a Profile 5 event protected at offset 64 passes the check, and reaches us without its E2E header.
    e1_profile5_event_at_offset_64,
    std = run,
    bare_metal = run,
    {
        let (server, mut rt) = subscribed(e2e_client());
        let [good, next] = protected_frames();
        server.fp.send_unicast(client_endpoint(), &good);
        let event = next_matching(&mut rt, DELIVERY_WAIT, is_event).unwrap_or_else(|seen| {
            panic!(
                "{}: no Event within {DELIVERY_WAIT:?} for a Profile 5 protected notification; \
                 observed: {seen:?}; datagram: {good:02X?}",
                Rt::NAME
            )
        });
        let Observation::Event {
            payload, e2e_ok, ..
        } = &event
        else {
            unreachable!("is_event matched {event:?}")
        };
        assert!(
            *e2e_ok == Some(true) && payload[..] == PAYLOAD,
            "{}: a Profile 5 protected notification (Data ID 0x{DATA_ID:04X}, counter 0) was \
             delivered with e2e_ok {e2e_ok:?} and payload {payload:02X?}; want Some(true) and \
             {PAYLOAD:02X?}, the payload without its 3-byte E2E header; datagram: {good:02X?}",
            Rt::NAME
        );
        // Negative control: the next frame, in sequence, with one CRC byte
        // corrupted, must fail the check, so the check above was not a pass
        // by default.
        let mut corrupted = next;
        corrupted[E2E_AT] ^= 0xFF;
        server.fp.send_unicast(client_endpoint(), &corrupted);
        match next_matching(&mut rt, QUIET, is_event) {
            Ok(Observation::Event {
                e2e_ok: Some(false),
                ..
            }) => eprintln!(
                "{}: the corrupted frame was delivered with e2e_ok Some(false)",
                Rt::NAME
            ),
            Err(_) => eprintln!("{}: the corrupted frame was not delivered", Rt::NAME),
            Ok(o) => panic!(
                "{}: control failed: a frame whose CRC is corrupted was reported as {o:?}; want \
                 e2e_ok Some(false), or no Event, so the E2E check is not running; datagram: \
                 {corrupted:02X?}",
                Rt::NAME
            ),
        }
    }
);

scenario!(
    /// PRS_E2E_00399–00401 / feat_req_someip_102 — a Profile 5 event that vsomeip protects passes our check.
    e1_profile5_event_at_offset_64_vsomeip,
    std = ignore("needs the vsomeip E2E plugin configured for Profile 5"),
    bare_metal = ignore("needs the vsomeip E2E plugin configured for Profile 5"),
    {
        // vsomeip sends `PAYLOAD` as given; its E2E plugin, configured for
        // Profile 5 with Data ID 0x9C6E on 0x1234/0x8001, would protect it.
        let mut peer = VsomeipPeer::start();
        peer.send(VSOMEIP_OFFER);
        let mut rt = Rt::start(e2e_client());
        let hex: String = PAYLOAD.iter().map(|b| format!("{b:02x}")).collect();
        let deadline = Instant::now() + SD_WAIT;
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            peer.send(&format!("notify 1234 0001 8001 {hex}"));
            let tick = (Instant::now() + Duration::from_millis(200)).min(deadline);
            while let Some(left) = tick.checked_duration_since(Instant::now()) {
                match rt.next(left) {
                    Some(Observation::Event {
                        service: SVC,
                        event: EVENT,
                        payload,
                        e2e_ok: Some(true),
                    }) if payload[..] == PAYLOAD => return,
                    Some(o) => seen.push(format!("{o:?}")),
                    None => break,
                }
            }
        }
        panic!(
            "{}: no Event with e2e_ok Some(true) and payload {PAYLOAD:02X?} within {SD_WAIT:?}; \
             observed: {seen:?}",
            Rt::NAME
        )
    }
);
