//! Phase 0 of the automotive-wire-codec migration: byte-exact "golden"
//! wire snapshots.
//!
//! Every test in this file asserts encoder output against a **hard-coded
//! `&[u8]` literal** derived by hand from the SOME/IP / SOME/IP-SD wire
//! layout — never a re-encode of the same value compared to itself. The
//! literal is the oracle. Each literal is cross-checked against the
//! current encoder (that's what running the test does); if a hand
//! derivation ever disagrees with the encoder, that is a latent wire bug
//! to report, not something to "fix" by copying encoder output into the
//! literal.
//!
//! These tests must keep passing, byte-for-byte, through every later
//! phase of the automotive-wire-codec migration — that is their entire
//! purpose.
//!
//! Known pre-existing bug (see `src/protocol/sd/entry.rs`): `Entry::
//! required_size()` returns 17 and `ServiceEntry`/`EventGroupEntry::
//! encode` return `Ok(16)` while only writing 15 bytes — i.e. the
//! *returned counts* overstate the true 16-byte wire size by one. This
//! affects only the `usize` returned from `encode`/`required_size`, not
//! the bytes actually placed on the wire, so it does not affect any
//! golden literal below (all literals reflect the true 16-byte
//! on-wire entry size). It is fixed in a later phase under the
//! protection of these golden tests.

use simple_someip::WireFormat;
use simple_someip::protocol::sd::{
    Entry, EventGroupEntry, Flags, Header as SdHeader, OptionType, Options, OptionsCount,
    RebootFlag, ServiceEntry, TransportProtocol,
};
use simple_someip::protocol::{Header, MessageId, MessageType, MessageTypeField, ReturnCode};

// ===========================================================================
// 1. SOME/IP `Header`
// ===========================================================================

#[test]
fn header_request_golden_bytes() {
    // service_id 0x1234, method_id 0x0001, no payload (length = 8),
    // request_id 0xABCD0042, protocol_version 0x01, interface_version 0x03,
    // message_type Request/no-TP (0x00), return_code Ok (0x00).
    let header = Header::new(
        MessageId::new_from_service_and_method(0x1234, 0x0001),
        0xABCD_0042,
        0x01,
        0x03,
        MessageTypeField::new(MessageType::Request, false),
        ReturnCode::Ok,
        0,
    );
    let mut buf = [0u8; 16];
    let n = header.encode_to_slice(&mut buf).unwrap();
    assert_eq!(n, 16);

    #[rustfmt::skip]
    let expected: [u8; 16] = [
        0x12, 0x34, 0x00, 0x01, // message_id: service 0x1234, method 0x0001
        0x00, 0x00, 0x00, 0x08, // length = 8 (payload_len 0 + 8)
        0xAB, 0xCD, 0x00, 0x42, // request_id
        0x01,                   // protocol_version
        0x03,                   // interface_version
        0x00,                   // message_type: Request, no TP
        0x00,                   // return_code: Ok
    ];
    assert_eq!(buf, expected);
}

#[test]
fn header_response_tp_generic_error_golden_bytes() {
    // A second header value exercising the Response wire byte (0x80,
    // NOT the enum discriminant), the TP flag, a non-Ok return code,
    // and a non-zero payload length feeding the length field.
    let header = Header::new(
        MessageId::new_from_service_and_method(0x005B, 0x8001),
        0x0000_0007,
        0x01,
        0x02,
        MessageTypeField::new(MessageType::Response, true),
        ReturnCode::GenericError(0x15),
        10,
    );
    let mut buf = [0u8; 16];
    header.encode_to_slice(&mut buf).unwrap();

    #[rustfmt::skip]
    let expected: [u8; 16] = [
        0x00, 0x5B, 0x80, 0x01, // message_id: service 0x005B, method 0x8001
        0x00, 0x00, 0x00, 0x12, // length = 18 (payload_len 10 + 8)
        0x00, 0x00, 0x00, 0x07, // request_id
        0x01,                   // protocol_version
        0x02,                   // interface_version
        0xA0,                   // message_type: Response (0x80) | TP flag (0x20)
        0x15,                   // return_code: GenericError(0x15)
    ];
    assert_eq!(buf, expected);
}

// ===========================================================================
// 2. `Message<P>` with a raw/opaque payload
// ===========================================================================

#[cfg(feature = "std")]
mod message_golden {
    use simple_someip::PayloadWireFormat;
    use simple_someip::RawPayload;
    use simple_someip::protocol::Message;

    use super::{Header, MessageId, MessageType, MessageTypeField, ReturnCode, WireFormat};

    #[test]
    fn message_raw_payload_golden_bytes() {
        // Non-SD message: service 0x005B, method 0x0001, 4-byte opaque
        // payload. header(16) + payload(4) = 20 bytes total.
        let payload_bytes = [0xDE, 0xAD, 0xBE, 0xEF];
        let message_id = MessageId::new_from_service_and_method(0x005B, 0x0001);
        let header = Header::new(
            message_id,
            0x0000_0001,
            0x01,
            0x01,
            MessageTypeField::new(MessageType::Request, false),
            ReturnCode::Ok,
            payload_bytes.len(),
        );
        let payload = RawPayload::from_payload_bytes(message_id, &payload_bytes).unwrap();
        let message = Message::new(header, payload);

        let mut buf = [0u8; 20];
        let n = message.encode_to_slice(&mut buf).unwrap();
        assert_eq!(n, 20);

        #[rustfmt::skip]
        let expected: [u8; 20] = [
            0x00, 0x5B, 0x00, 0x01, // message_id: service 0x005B, method 0x0001
            0x00, 0x00, 0x00, 0x0C, // length = 12 (payload_len 4 + 8)
            0x00, 0x00, 0x00, 0x01, // request_id
            0x01,                   // protocol_version
            0x01,                   // interface_version
            0x00,                   // message_type: Request, no TP
            0x00,                   // return_code: Ok
            0xDE, 0xAD, 0xBE, 0xEF, // opaque payload bytes
        ];
        assert_eq!(buf, expected);
    }
}

// ===========================================================================
// 3. SD `Header` — one composite per `EntryType`, one test per `OptionType`
// ===========================================================================

#[test]
fn sd_header_find_service_golden_bytes() {
    // FindService entry with vsomeip-standard wildcard instance/version
    // fields, no options.
    let entries = [Entry::FindService(ServiceEntry::find(0x1234))];
    let sd_header = SdHeader::new(Flags::new_sd(RebootFlag::RecentlyRebooted), &entries, &[]);

    let mut buf = [0u8; 28];
    let n = sd_header.encode_to_slice(&mut buf).unwrap();
    assert_eq!(n, 28);

    #[rustfmt::skip]
    let expected: [u8; 28] = [
        0xC0, 0x00, 0x00, 0x00, // flags (reboot|unicast) + 3 reserved bytes
        0x00, 0x00, 0x00, 0x10, // entries_size = 16 (1 entry)
        // --- FindService entry (16 bytes) ---
        0x00,                   // entry type: FindService
        0x00, 0x00,             // index_first/second_options_run
        0x10,                   // options_count: first=1, second=0
        0x12, 0x34,             // service_id
        0xFF, 0xFF,             // instance_id: wildcard
        0xFF,                   // major_version: wildcard
        0xFF, 0xFF, 0xFF,       // ttl: wildcard (0x00FFFFFF)
        0xFF, 0xFF, 0xFF, 0xFF, // minor_version: wildcard
        0x00, 0x00, 0x00, 0x00, // options_size = 0
    ];
    assert_eq!(buf, expected);
}

#[test]
fn sd_header_offer_service_with_ipv4_endpoint_golden_bytes() {
    // OfferService entry referencing one IPv4Endpoint option.
    let entry = Entry::OfferService(ServiceEntry {
        index_first_options_run: 0,
        index_second_options_run: 0,
        options_count: OptionsCount::new(1, 0),
        service_id: 0x1234,
        instance_id: 0x0001,
        major_version: 1,
        ttl: 0x00FF_FFFF,
        minor_version: 0,
    });
    let option = Options::IpV4Endpoint {
        ip: core::net::Ipv4Addr::new(192, 168, 1, 10),
        protocol: TransportProtocol::Udp,
        port: 30509,
    };
    let entries = [entry];
    let options = [option];
    let sd_header = SdHeader::new(
        Flags::new_sd(RebootFlag::RecentlyRebooted),
        &entries,
        &options,
    );

    let mut buf = [0u8; 40];
    let n = sd_header.encode_to_slice(&mut buf).unwrap();
    assert_eq!(n, 40);

    #[rustfmt::skip]
    let expected: [u8; 40] = [
        0xC0, 0x00, 0x00, 0x00, // flags + reserved
        0x00, 0x00, 0x00, 0x10, // entries_size = 16
        // --- OfferService entry (16 bytes) ---
        0x01,                   // entry type: OfferService
        0x00, 0x00,             // index_first/second_options_run
        0x10,                   // options_count: first=1, second=0
        0x12, 0x34,             // service_id
        0x00, 0x01,             // instance_id
        0x01,                   // major_version
        0xFF, 0xFF, 0xFF,       // ttl = 0x00FFFFFF
        0x00, 0x00, 0x00, 0x00, // minor_version
        0x00, 0x00, 0x00, 0x0C, // options_size = 12
        // --- IPv4Endpoint option (12 bytes) ---
        0x00, 0x09,             // length field = 9
        0x04,                   // option type: IpV4Endpoint
        0x00,                   // discard flag
        192, 168, 1, 10,        // ip
        0x00,                   // reserved
        0x11,                   // protocol: UDP
        0x77, 0x2D,             // port = 30509
    ];
    assert_eq!(buf, expected);
}

#[test]
fn sd_header_stop_offer_service_with_load_balancing_golden_bytes() {
    // StopOfferService entry (TTL forced to 0 by convention) referencing
    // one LoadBalancing option.
    let entry = Entry::StopOfferService(ServiceEntry {
        index_first_options_run: 0,
        index_second_options_run: 0,
        options_count: OptionsCount::new(1, 0),
        service_id: 0xABCD,
        instance_id: 0x0002,
        major_version: 2,
        ttl: 0,
        minor_version: 1,
    });
    let option = Options::LoadBalancing {
        priority: 100,
        weight: 200,
    };
    let entries = [entry];
    let options = [option];
    let sd_header = SdHeader::new(Flags::new_sd(RebootFlag::Continuous), &entries, &options);

    let mut buf = [0u8; 36];
    let n = sd_header.encode_to_slice(&mut buf).unwrap();
    assert_eq!(n, 36);

    #[rustfmt::skip]
    let expected: [u8; 36] = [
        0x40, 0x00, 0x00, 0x00, // flags: unicast only (Continuous reboot)
        0x00, 0x00, 0x00, 0x10, // entries_size = 16
        // --- StopOfferService entry (16 bytes) ---
        0x02,                   // entry type: StopOfferService
        0x00, 0x00,             // index_first/second_options_run
        0x10,                   // options_count: first=1, second=0
        0xAB, 0xCD,             // service_id
        0x00, 0x02,             // instance_id
        0x02,                   // major_version
        0x00, 0x00, 0x00,       // ttl = 0 (stop-offer)
        0x00, 0x00, 0x00, 0x01, // minor_version
        0x00, 0x00, 0x00, 0x08, // options_size = 8
        // --- LoadBalancing option (8 bytes) ---
        0x00, 0x05,             // length field = 5
        0x02,                   // option type: LoadBalancing
        0x00,                   // discard flag
        0x00, 0x64,             // priority = 100
        0x00, 0xC8,             // weight = 200
    ];
    assert_eq!(buf, expected);
}

#[test]
fn sd_header_subscribe_eventgroup_with_ipv6_endpoint_golden_bytes() {
    // Subscribe (eventgroup) entry referencing one IPv6Endpoint option.
    let entry = Entry::SubscribeEventGroup(EventGroupEntry {
        index_first_options_run: 0,
        index_second_options_run: 0,
        options_count: OptionsCount::new(1, 0),
        service_id: 0x0042,
        instance_id: 0x0001,
        major_version: 1,
        ttl: 3,
        counter: 0,
        event_group_id: 1,
    });
    let option = Options::IpV6Endpoint {
        ip: core::net::Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1),
        protocol: TransportProtocol::Tcp,
        port: 8080,
    };
    let entries = [entry];
    let options = [option];
    let sd_header = SdHeader::new(
        Flags::new_sd(RebootFlag::RecentlyRebooted),
        &entries,
        &options,
    );

    let mut buf = [0u8; 52];
    let n = sd_header.encode_to_slice(&mut buf).unwrap();
    assert_eq!(n, 52);

    #[rustfmt::skip]
    let expected: [u8; 52] = [
        0xC0, 0x00, 0x00, 0x00, // flags + reserved
        0x00, 0x00, 0x00, 0x10, // entries_size = 16
        // --- Subscribe entry (16 bytes) ---
        0x06,                   // entry type: Subscribe
        0x00, 0x00,             // index_first/second_options_run
        0x10,                   // options_count: first=1, second=0
        0x00, 0x42,             // service_id
        0x00, 0x01,             // instance_id
        0x01,                   // major_version
        0x00, 0x00, 0x03,       // ttl = 3
        0x00, 0x00,             // counter = 0
        0x00, 0x01,             // event_group_id = 1
        0x00, 0x00, 0x00, 0x18, // options_size = 24
        // --- IPv6Endpoint option (24 bytes) ---
        0x00, 0x15,             // length field = 21
        0x06,                   // option type: IpV6Endpoint
        0x00,                   // discard flag
        0xFE, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, // ip: fe80::1
        0x00,                   // reserved
        0x06,                   // protocol: TCP
        0x1F, 0x90,             // port = 8080
    ];
    assert_eq!(buf, expected);
}

#[test]
fn sd_header_subscribe_ack_eventgroup_with_configuration_golden_bytes() {
    // SubscribeAckEventGroup entry referencing one Configuration option.
    let entry = Entry::SubscribeAckEventGroup(EventGroupEntry {
        index_first_options_run: 0,
        index_second_options_run: 0,
        options_count: OptionsCount::new(1, 0),
        service_id: 0xAAAA,
        instance_id: 0x0001,
        major_version: 1,
        ttl: 0x00FF_FFFF,
        counter: 0,
        event_group_id: 0x0010,
    });
    let mut configuration_string =
        heapless::Vec::<u8, { simple_someip::protocol::sd::MAX_CONFIGURATION_STRING_LENGTH }>::new(
        );
    configuration_string.extend_from_slice(b"abc").unwrap();
    let option = Options::Configuration {
        configuration_string,
    };
    let entries = [entry];
    let options = [option];
    let sd_header = SdHeader::new(Flags::new_sd(RebootFlag::Continuous), &entries, &options);

    let mut buf = [0u8; 35];
    let n = sd_header.encode_to_slice(&mut buf).unwrap();
    assert_eq!(n, 35);

    #[rustfmt::skip]
    let expected: [u8; 35] = [
        0x40, 0x00, 0x00, 0x00, // flags: unicast only
        0x00, 0x00, 0x00, 0x10, // entries_size = 16
        // --- SubscribeAck entry (16 bytes) ---
        0x07,                   // entry type: SubscribeAck
        0x00, 0x00,             // index_first/second_options_run
        0x10,                   // options_count: first=1, second=0
        0xAA, 0xAA,             // service_id
        0x00, 0x01,             // instance_id
        0x01,                   // major_version
        0xFF, 0xFF, 0xFF,       // ttl = 0x00FFFFFF
        0x00, 0x00,             // counter = 0
        0x00, 0x10,             // event_group_id = 0x0010
        0x00, 0x00, 0x00, 0x07, // options_size = 7
        // --- Configuration option (7 bytes) ---
        0x00, 0x04,             // length field = 4 (1 + string_len 3)
        0x01,                   // option type: Configuration
        0x00,                   // discard flag
        0x61, 0x62, 0x63,       // "abc"
    ];
    assert_eq!(buf, expected);
}

// --- One standalone test per `OptionType` (8 variants) ---

fn encode_option(option: &Options) -> heapless::Vec<u8, 32> {
    let size = option.size();
    let mut buf = [0u8; 32];
    let n = option.write(&mut &mut buf[..size]).unwrap();
    assert_eq!(n, size);
    let mut out = heapless::Vec::new();
    out.extend_from_slice(&buf[..size]).unwrap();
    out
}

#[test]
fn option_configuration_golden_bytes() {
    let mut configuration_string =
        heapless::Vec::<u8, { simple_someip::protocol::sd::MAX_CONFIGURATION_STRING_LENGTH }>::new(
        );
    configuration_string.extend_from_slice(b"ab").unwrap();
    let option = Options::Configuration {
        configuration_string,
    };
    assert_eq!(u8::from(OptionType::Configuration), 0x01);
    let bytes = encode_option(&option);
    // length=3 (1 + string_len 2), type=0x01, discard=0, "ab"
    assert_eq!(bytes.as_slice(), &[0x00, 0x03, 0x01, 0x00, 0x61, 0x62]);
}

#[test]
fn option_load_balancing_golden_bytes() {
    let option = Options::LoadBalancing {
        priority: 0x1234,
        weight: 0x5678,
    };
    assert_eq!(u8::from(OptionType::LoadBalancing), 0x02);
    let bytes = encode_option(&option);
    // length=5, type=0x02, discard=0, priority=0x1234, weight=0x5678
    assert_eq!(
        bytes.as_slice(),
        &[0x00, 0x05, 0x02, 0x00, 0x12, 0x34, 0x56, 0x78]
    );
}

#[test]
fn option_ipv4_endpoint_golden_bytes() {
    let option = Options::IpV4Endpoint {
        ip: core::net::Ipv4Addr::new(10, 0, 0, 1),
        protocol: TransportProtocol::Udp,
        port: 30490,
    };
    assert_eq!(u8::from(OptionType::IpV4Endpoint), 0x04);
    let bytes = encode_option(&option);
    // length=9, type=0x04, discard=0, ip=10.0.0.1, reserved=0, proto=UDP(0x11), port=30490
    assert_eq!(
        bytes.as_slice(),
        &[0x00, 0x09, 0x04, 0x00, 10, 0, 0, 1, 0x00, 0x11, 0x77, 0x1A]
    );
}

#[test]
fn option_ipv6_endpoint_golden_bytes() {
    let option = Options::IpV6Endpoint {
        ip: core::net::Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1),
        protocol: TransportProtocol::Tcp,
        port: 443,
    };
    assert_eq!(u8::from(OptionType::IpV6Endpoint), 0x06);
    let bytes = encode_option(&option);
    // length=21, type=0x06, discard=0, ip=2001:db8::1, reserved=0, proto=TCP(0x06), port=443
    #[rustfmt::skip]
    let expected: [u8; 24] = [
        0x00, 0x15, 0x06, 0x00,
        0x20, 0x01, 0x0D, 0xB8, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
        0x00, 0x06, 0x01, 0xBB,
    ];
    assert_eq!(bytes.as_slice(), &expected);
}

#[test]
fn option_ipv4_multicast_golden_bytes() {
    let option = Options::IpV4Multicast {
        ip: core::net::Ipv4Addr::new(239, 1, 2, 3),
        protocol: TransportProtocol::Udp,
        port: 30491,
    };
    assert_eq!(u8::from(OptionType::IpV4Multicast), 0x14);
    let bytes = encode_option(&option);
    // length=9, type=0x14, discard=0, ip=239.1.2.3, reserved=0, proto=UDP(0x11), port=30491
    assert_eq!(
        bytes.as_slice(),
        &[0x00, 0x09, 0x14, 0x00, 239, 1, 2, 3, 0x00, 0x11, 0x77, 0x1B]
    );
}

#[test]
fn option_ipv6_multicast_golden_bytes() {
    let option = Options::IpV6Multicast {
        ip: core::net::Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xabcd),
        protocol: TransportProtocol::Udp,
        port: 5353,
    };
    assert_eq!(u8::from(OptionType::IpV6Multicast), 0x16);
    let bytes = encode_option(&option);
    // length=21, type=0x16, discard=0, ip=ff02::abcd, reserved=0, proto=UDP(0x11), port=5353
    #[rustfmt::skip]
    let expected: [u8; 24] = [
        0x00, 0x15, 0x16, 0x00,
        0xFF, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xAB, 0xCD,
        0x00, 0x11, 0x14, 0xE9,
    ];
    assert_eq!(bytes.as_slice(), &expected);
}

#[test]
fn option_ipv4_sd_golden_bytes() {
    let option = Options::IpV4SD {
        ip: core::net::Ipv4Addr::new(192, 168, 0, 100),
        protocol: TransportProtocol::Udp,
        port: 30490,
    };
    assert_eq!(u8::from(OptionType::IpV4SD), 0x24);
    let bytes = encode_option(&option);
    // length=9, type=0x24, discard=0, ip=192.168.0.100, reserved=0, proto=UDP(0x11), port=30490
    assert_eq!(
        bytes.as_slice(),
        &[
            0x00, 0x09, 0x24, 0x00, 192, 168, 0, 100, 0x00, 0x11, 0x77, 0x1A
        ]
    );
}

#[test]
fn option_ipv6_sd_golden_bytes() {
    let option = Options::IpV6SD {
        ip: core::net::Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0xabcd, 0xef01),
        protocol: TransportProtocol::Tcp,
        port: 30490,
    };
    assert_eq!(u8::from(OptionType::IpV6SD), 0x26);
    let bytes = encode_option(&option);
    // length=21, type=0x26, discard=0, ip=fe80::abcd:ef01, reserved=0, proto=TCP(0x06), port=30490
    #[rustfmt::skip]
    let expected: [u8; 24] = [
        0x00, 0x15, 0x26, 0x00,
        0xFE, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0xAB, 0xCD, 0xEF, 0x01,
        0x00, 0x06, 0x77, 0x1A,
    ];
    assert_eq!(bytes.as_slice(), &expected);
}

// ===========================================================================
// 4. `sd_codec` datagram builders
// ===========================================================================

#[cfg(any(feature = "bare_metal", feature = "server"))]
mod sd_codec_golden {
    use simple_someip::protocol::sd::RebootFlag;
    use simple_someip::sd_codec::{
        OfferServiceRequest, SubscribeAckRequest, SubscribeEventgroupRequest,
        build_offer_service_datagram, build_subscribe_ack_datagram,
        build_subscribe_eventgroup_datagram,
    };

    // NOTE: `encode_sd_datagram` (src/sd_codec.rs ~line 264) is a private
    // helper, not reachable from an external integration test. Every
    // public `build_*` builder funnels through it (SOME/IP wrapper via
    // `Header::new_sd` + SD payload via `sd::Header::encode`), so pinning
    // the three builders below exercises exactly the same encode path
    // `encode_sd_datagram` would.

    #[test]
    fn build_subscribe_eventgroup_datagram_golden_bytes() {
        let request = SubscribeEventgroupRequest {
            service_id: 0x1234,
            instance_id: 0x0001,
            major_version: 1,
            event_group_id: 0x0001,
            ttl: 5,
            local_ip: core::net::Ipv4Addr::new(10, 0, 0, 5),
            local_rx_port: 30509,
        };
        let mut buf = [0u8; 64];
        let n = build_subscribe_eventgroup_datagram(&mut buf, &request, 7, RebootFlag::Continuous)
            .unwrap();
        assert_eq!(n, 56);

        #[rustfmt::skip]
        let expected: [u8; 56] = [
            // --- SOME/IP header (16 bytes) ---
            0xFF, 0xFF, 0x81, 0x00, // message_id: SD (service 0xFFFF, method 0x8100)
            0x00, 0x00, 0x00, 0x30, // length = 48 (SD payload 40 + 8)
            0x00, 0x00, 0x00, 0x07, // request_id = session 7
            0x01, 0x01,             // protocol/interface version
            0x02, 0x00,             // message_type Notification, return_code Ok
            // --- SD payload (40 bytes) ---
            0x40, 0x00, 0x00, 0x00, // flags: unicast only (Continuous reboot)
            0x00, 0x00, 0x00, 0x10, // entries_size = 16
            // Subscribe entry (16 bytes)
            0x06, 0x00, 0x00, 0x10, 0x12, 0x34, 0x00, 0x01,
            0x01, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x01,
            0x00, 0x00, 0x00, 0x0C, // options_size = 12
            // IPv4Endpoint option (12 bytes): 10.0.0.5:30509/UDP
            0x00, 0x09, 0x04, 0x00, 10, 0, 0, 5, 0x00, 0x11, 0x77, 0x2D,
        ];
        assert_eq!(buf[..n], expected);
    }

    #[test]
    fn build_offer_service_datagram_golden_bytes() {
        let request = OfferServiceRequest {
            service_id: 0x0042,
            instance_id: 0x0001,
            major_version: 1,
            minor_version: 0,
            ttl: 3,
            local_ip: core::net::Ipv4Addr::new(192, 0, 2, 1),
            unicast_port: 30501,
        };
        let mut buf = [0u8; 64];
        let n = build_offer_service_datagram(&mut buf, &request, 1).unwrap();
        assert_eq!(n, 56);

        #[rustfmt::skip]
        let expected: [u8; 56] = [
            // --- SOME/IP header (16 bytes) ---
            0xFF, 0xFF, 0x81, 0x00,
            0x00, 0x00, 0x00, 0x30, // length = 48
            0x00, 0x00, 0x00, 0x01, // request_id = session 1
            0x01, 0x01, 0x02, 0x00,
            // --- SD payload (40 bytes) ---
            0x40, 0x00, 0x00, 0x00, // flags: unicast only (builder forces Continuous)
            0x00, 0x00, 0x00, 0x10,
            // OfferService entry (16 bytes)
            0x01, 0x00, 0x00, 0x10, 0x00, 0x42, 0x00, 0x01,
            0x01, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x0C, // options_size = 12
            // IPv4Endpoint option (12 bytes): 192.0.2.1:30501/UDP
            0x00, 0x09, 0x04, 0x00, 192, 0, 2, 1, 0x00, 0x11, 0x77, 0x25,
        ];
        assert_eq!(buf[..n], expected);
    }

    #[test]
    fn build_subscribe_ack_datagram_golden_bytes() {
        let request = SubscribeAckRequest {
            service_id: 0x0099,
            instance_id: 2,
            event_group_id: 0x0005,
            major_version: 1,
            ttl: 10,
        };
        let mut buf = [0u8; 64];
        let n = build_subscribe_ack_datagram(&mut buf, &request, 2).unwrap();
        assert_eq!(n, 44);

        #[rustfmt::skip]
        let expected: [u8; 44] = [
            // --- SOME/IP header (16 bytes) ---
            0xFF, 0xFF, 0x81, 0x00,
            0x00, 0x00, 0x00, 0x24, // length = 36 (SD payload 28 + 8)
            0x00, 0x00, 0x00, 0x02, // request_id = session 2
            0x01, 0x01, 0x02, 0x00,
            // --- SD payload (28 bytes), no options ---
            0x40, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x10,
            // SubscribeAckEventGroup entry (16 bytes), options_count = (0, 0)
            0x07, 0x00, 0x00, 0x00, 0x00, 0x99, 0x00, 0x02,
            0x01, 0x00, 0x00, 0x0A, 0x00, 0x00, 0x00, 0x05,
            0x00, 0x00, 0x00, 0x00, // options_size = 0
        ];
        assert_eq!(buf[..n], expected);
    }
}
