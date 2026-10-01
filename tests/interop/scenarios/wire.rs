//! Wire-format scenarios: the bytes the runtime under test puts on the wire.

use crate::Rt;
use crate::interop::consts::*;
use crate::interop::peers::frames::{FramePeer, parse};
use crate::interop::runtime::{Offer, Setup, SomeipUnderTest};

use super::SD_WAIT;

const OFFER_SERVICE: u8 = 0x01;
const SUBSCRIBE_EVENTGROUP: u8 = 0x06;
const IPV4_ENDPOINT: u8 = 0x04;
const UDP: u8 = 0x11;

fn offer() -> Offer {
    Offer {
        ttl_s: 3,
        events: vec![EVENT],
        fields: vec![],
        methods: vec![METHOD],
    }
}

fn u16_at(d: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([d[at], d[at + 1]])
}

fn u24_at(d: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([0, d[at], d[at + 1], d[at + 2]])
}

fn u32_at(d: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([d[at], d[at + 1], d[at + 2], d[at + 3]])
}

/// Field-by-field comparison that reports every mismatch at once.
#[derive(Default)]
struct Fields(Vec<String>);

impl Fields {
    fn check<T: PartialEq + std::fmt::Debug>(&mut self, name: &str, got: T, want: T) {
        if got != want {
            self.0.push(format!("{name}: got {got:?}, want {want:?}"));
        }
    }
}

scenario!(
    /// PRS_SOMEIPSD_00151–00164 (SD header), 00254–00261 (flags), 00307 (IPv4 Endpoint Option) — our offer's bytes match the specification.
    w1_offer_wire_format,
    std = run,
    bare_metal = run,
    {
        let peer = FramePeer::start();
        let _rt = Rt::start(Setup {
            offer: Some(offer()),
            consume: None,
        });
        let d = peer
            .recv_sd(SD_WAIT, |d| {
                parse::sd_entry_types(d)
                    .iter()
                    .any(|&(t, _)| t == OFFER_SERVICE)
            })
            .unwrap_or_else(|| panic!("{}: no OfferService within {SD_WAIT:?}", Rt::NAME));

        // SOME/IP header (16) + SD flags and lengths (12) + one entry (16)
        // + one IPv4 endpoint option (12).
        const LEN: usize = 56;
        assert!(
            d.len() >= LEN,
            "{}: offer is {} bytes, want {LEN}: {d:02X?}",
            Rt::NAME,
            d.len()
        );
        let mut f = Fields::default();
        f.check("datagram length", d.len(), LEN);

        // SOME/IP header.
        f.check("message ID", u32_at(&d, 0), 0xFFFF_8100);
        f.check("length field", u32_at(&d, 4) as usize, d.len() - 8);
        f.check("client ID", u16_at(&d, 8), 0);
        f.check("protocol version", d[12], 1);
        f.check("interface version", d[13], 1);
        f.check("message type", d[14], 0x02);
        f.check("return code", d[15], 0x00);

        // SD flags: the runtime has just started, so its session counter
        // has not wrapped.
        f.check("reboot flag", d[16] & 0x80 != 0, true);
        f.check("unicast flag", d[16] & 0x40 != 0, true);
        f.check("entries length", u32_at(&d, 20), 16);

        // The one OfferService entry.
        let e = &d[24..40];
        f.check("entry type", e[0], OFFER_SERVICE);
        f.check("index of first options run", e[1], 0);
        f.check("options in first run", e[3] >> 4, 1);
        f.check("options in second run", e[3] & 0x0F, 0);
        f.check("service ID", u16_at(e, 4), SVC);
        f.check("instance ID", u16_at(e, 6), INST);
        f.check("major version", e[8], MAJOR);
        f.check("TTL", u24_at(e, 9), 3);
        f.check("minor version", u32_at(e, 12), 0);

        // The one IPv4 endpoint option.
        f.check("options length", u32_at(&d, 40), 12);
        let o = &d[44..56];
        f.check("option length", u16_at(o, 0), 9);
        f.check("option type", o[2], IPV4_ENDPOINT);
        f.check("endpoint address", [o[4], o[5], o[6], o[7]], OUR_IP.octets());
        f.check("endpoint L4 protocol", o[9], UDP);
        f.check("endpoint port", u16_at(o, 10), SERVER_PORT);

        assert!(
            f.0.is_empty(),
            "{}: offer does not match the SD wire format:\n  {}\ndatagram: {d:02X?}",
            Rt::NAME,
            f.0.join("\n  ")
        );
    }
);

/// The builders, checked against simple-someip's SD decoder.
#[cfg(not(feature = "bare-metal-runtime"))]
mod builders {
    use simple_someip::protocol::sd::{
        Entry, EventGroupEntry, Options, OptionsCount, RebootFlag, ServiceEntry, TransportProtocol,
    };
    use simple_someip::protocol::{MessageType, MessageView, ReturnCode};

    use super::*;
    use crate::interop::peers::frames::build;

    #[test]
    fn an_offer_decodes_field_for_field() {
        let d = build::sd_message(
            1,
            true,
            true,
            &[build::service_entry(
                OFFER_SERVICE,
                0,
                0,
                1,
                0,
                SVC,
                INST,
                MAJOR,
                3,
                0,
            )],
            &[build::ipv4_endpoint_option(PEER_IP, UDP, SERVER_PORT)],
        );

        let view = MessageView::parse(&d).expect("the offer parses as SOME/IP");
        let header = view.header();
        assert_eq!(header.message_id().service_id(), 0xFFFF);
        assert_eq!(header.message_id().method_id(), 0x8100);
        assert_eq!(header.length() as usize, d.len() - 8);
        assert_eq!(header.request_id(), 0x0000_0001);
        assert_eq!(header.protocol_version(), 1);
        assert_eq!(header.interface_version(), 1);
        assert_eq!(
            header.message_type().message_type(),
            MessageType::Notification
        );
        assert_eq!(header.return_code(), ReturnCode::Ok);

        let sd = view.sd_header().expect("the offer parses as SD");
        assert_eq!(sd.flags().reboot(), RebootFlag::RecentlyRebooted);
        assert!(sd.flags().unicast());
        let entries: Vec<Entry> = sd.entries().map(|e| e.to_owned().unwrap()).collect();
        assert_eq!(
            entries,
            [Entry::OfferService(ServiceEntry {
                index_first_options_run: 0,
                index_second_options_run: 0,
                options_count: OptionsCount::new(1, 0),
                service_id: SVC,
                instance_id: INST,
                major_version: MAJOR,
                ttl: 3,
                minor_version: 0,
            })]
        );
        let options: Vec<Options> = sd.options().map(|o| o.to_owned().unwrap()).collect();
        assert_eq!(
            options,
            [Options::IpV4Endpoint {
                ip: PEER_IP,
                protocol: TransportProtocol::Udp,
                port: SERVER_PORT,
            }]
        );

        assert_eq!(parse::sd_entry_types(&d), [(OFFER_SERVICE, 3)]);
    }

    #[test]
    fn a_subscribe_decodes_field_for_field() {
        let d = build::sd_message(
            0x1234,
            false,
            true,
            &[build::eventgroup_entry(
                SUBSCRIBE_EVENTGROUP,
                0,
                1,
                SVC,
                INST,
                MAJOR,
                0x00AB_CDEF,
                true,
                2,
                EG,
            )],
            &[build::ipv4_endpoint_option(PEER_IP, UDP, CLIENT_PORT)],
        );

        let view = MessageView::parse(&d).expect("the subscribe parses as SOME/IP");
        assert_eq!(view.header().request_id(), 0x0000_1234);
        let sd = view.sd_header().expect("the subscribe parses as SD");
        assert_eq!(sd.flags().reboot(), RebootFlag::Continuous);
        let entries: Vec<Entry> = sd.entries().map(|e| e.to_owned().unwrap()).collect();
        assert_eq!(
            entries,
            [Entry::SubscribeEventGroup(EventGroupEntry {
                index_first_options_run: 0,
                index_second_options_run: 0,
                options_count: OptionsCount::new(1, 0),
                service_id: SVC,
                instance_id: INST,
                major_version: MAJOR,
                ttl: 0x00AB_CDEF,
                counter: 2,
                event_group_id: EG,
            })]
        );

        // The decoder does not expose the Initial Data Requested flag, so
        // read its byte directly: the flag in bit 7, the counter below it.
        assert_eq!(d[24 + 13], 0x82);

        assert_eq!(
            parse::sd_entry_types(&d),
            [(SUBSCRIBE_EVENTGROUP, 0x00AB_CDEF)]
        );
    }

    #[test]
    fn an_unknown_option_keeps_its_bytes() {
        let o = build::raw_option(0x77, &[0xDE, 0xAD]);
        assert_eq!(o, [0x00, 0x03, 0x77, 0x00, 0xDE, 0xAD]);
    }

    #[test]
    fn a_non_sd_datagram_has_no_entries() {
        let mut d = build::someip_header(SVC, METHOD, 0x0001, 0x0001, 0x00, 0x00, 4);
        d.extend([0; 4]);
        assert_eq!(parse::sd_entry_types(&d), []);
        assert_eq!(parse::sd_entry_types(&d[..10]), []);
    }
}
