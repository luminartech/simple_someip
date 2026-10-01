//! Interop tests for the bare-metal runtime (probe: the runtime starts and
//! emits an OfferService on a host transport).
#![cfg(feature = "bare-metal-runtime")]

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use simple_someip::bare_metal_runtime as rt;

static MAILBOX: rt::RxMailbox<{ rt::RX_SLOTS }, { rt::RX_CAP }> = rt::RxMailbox::new();
static mut BUFFERS: rt::RuntimeBuffers = rt::RuntimeBuffers::new();
static OFFERS: [rt::OfferEntry; 1] = [rt::OfferEntry {
    service_id: 0x1234,
    instance_id: 0x0001,
    event_group_id: 0x0001,
    unicast_port: 30509,
    major_version: 1,
    ttl_seconds: 3,
}];
static TX: OnceLock<UdpSocket> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();

extern "C" fn send(_local: u16, buf: *const u8, len: usize, dst: u32, dst_port: u16) -> i32 {
    let bytes = unsafe { std::slice::from_raw_parts(buf, len) };
    let sock = TX.get_or_init(|| UdpSocket::bind("127.0.0.1:0").unwrap());
    let to = SocketAddrV4::new(Ipv4Addr::from(dst), dst_port);
    sock.send_to(bytes, to).map_or(-1, |n| n as i32)
}
extern "C" fn bind(_port: u16, _is_sd: bool, _mcast: u32) -> i32 {
    0
}
extern "C" fn now_ms() -> u32 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u32
}
fn dispatch(_: usize, _: SocketAddrV4, _: u16, _: u16, _: &[u8], _: u8, _: &mut [u8]) -> i32 {
    0
}

#[test]
fn runtime_emits_offer_on_host() {
    // Listen where the runtime sends its offer: the SD group and port.
    let rx = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, None).unwrap();
    rx.set_reuse_address(true).unwrap();
    rx.set_reuse_port(true).unwrap();
    rx.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 30490).into())
        .unwrap();
    rx.join_multicast_v4(&Ipv4Addr::new(239, 255, 0, 255), &Ipv4Addr::LOCALHOST)
        .unwrap();
    let rx: UdpSocket = rx.into();
    rx.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();

    let rc = rt::init(rt::RuntimeConfig {
        interface: u32::from(Ipv4Addr::LOCALHOST),
        sd_port: 0,
        sd_mcast: 0,
        multicast_loopback: true,
        send,
        now_ms,
        dispatch,
        dispatch_ctx: 0,
        bind,
        offers: &OFFERS,
        subscriptions: &[],
        mailbox: &MAILBOX,
        buffers: unsafe { &mut *core::ptr::addr_of_mut!(BUFFERS) },
    });
    assert_eq!(rc, 0, "bare-metal runtime init failed on host");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut buf = [0u8; 1500];
    while Instant::now() < deadline {
        rt::poll();
        if let Ok(n) = rx.recv(&mut buf) {
            // SOME/IP-SD: Message ID 0xFFFF8100; the first entry's type (0x01 = Offer)
            // follows the 16-byte header, 4 bytes of flags/reserved and the 4-byte entries length.
            if n > 40 && buf[0..4] == [0xFF, 0xFF, 0x81, 0x00] && buf[24] == 0x01 {
                rt::deinit();
                return;
            }
        }
    }
    panic!("no OfferService seen from the bare-metal runtime within 5 s");
}
