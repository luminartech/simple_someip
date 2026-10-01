//! A scripted peer that sends hand-built SOME/IP and SOME/IP-SD datagrams
//! from `PEER_IP` and captures what the runtime under test sends back.
//!
//! Every byte the peer sends comes from [`build`], written from the wire
//! layouts in the specification rather than from simple-someip's encoder,
//! so a scenario can produce well-formed traffic, traffic no other
//! implementation produces, or deliberately malformed frames.

use std::io::ErrorKind;
use std::net::{SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

use crate::interop::consts::*;

/// How long `recv_sd` waits on one SD socket before checking the other.
const POLL: Duration = Duration::from_millis(10);

/// Large enough for any UDP datagram.
const MAX_DATAGRAM: usize = 65_535;

/// The frame peer's sockets.
///
/// SD traffic arrives on two sockets. Multicast is received on a socket
/// bound to the SD group address itself: a socket bound to `0.0.0.0` would
/// join the runtime's own `SO_REUSEPORT` group on the SD port, and the
/// kernel would then hand it a share of the unicast SD meant for the
/// runtime. Unicast SD addressed to the peer arrives on the socket bound to
/// `PEER_IP`, which also sends all of the peer's SD so that every datagram
/// leaves from `PEER_IP:SD_PORT`.
pub struct FramePeer {
    /// Bound to `PEER_IP:SD_PORT`: sends SD and receives unicast SD.
    sd: UdpSocket,
    /// Bound to `SD_GROUP:SD_PORT` and joined on `OUR_IP`: receives
    /// multicast SD.
    sd_group: UdpSocket,
    /// Bound to `PEER_IP:SERVER_PORT`: the peer's service endpoint.
    uni: UdpSocket,
}

impl FramePeer {
    /// Binds the peer's sockets; panics if any of them cannot be bound.
    pub fn start() -> Self {
        let sd = reusable(SocketAddrV4::new(PEER_IP, SD_PORT))
            .unwrap_or_else(|e| panic!("frame peer: could not bind {PEER_IP}:{SD_PORT}: {e}"));
        if sd.set_multicast_if_v4(&PEER_IP).is_err() {
            sd.set_multicast_if_v4(&OUR_IP).unwrap_or_else(|e| {
                panic!("frame peer: could not set the multicast interface: {e}")
            });
        }
        sd.set_multicast_loop_v4(true)
            .unwrap_or_else(|e| panic!("frame peer: could not enable multicast loopback: {e}"));
        let sd = UdpSocket::from(sd);

        let sd_group = reusable(SocketAddrV4::new(SD_GROUP, SD_PORT))
            .unwrap_or_else(|e| panic!("frame peer: could not bind {SD_GROUP}:{SD_PORT}: {e}"));
        sd_group
            .join_multicast_v4(&SD_GROUP, &OUR_IP)
            .unwrap_or_else(|e| panic!("frame peer: could not join {SD_GROUP} on {OUR_IP}: {e}"));
        sd_group
            .set_multicast_loop_v4(true)
            .unwrap_or_else(|e| panic!("frame peer: could not enable multicast loopback: {e}"));
        let sd_group = UdpSocket::from(sd_group);

        let uni = UdpSocket::bind(SocketAddrV4::new(PEER_IP, SERVER_PORT))
            .unwrap_or_else(|e| panic!("frame peer: could not bind {PEER_IP}:{SERVER_PORT}: {e}"));
        Self { sd, sd_group, uni }
    }

    /// Sends `datagram` to the SD multicast group.
    pub fn send_sd_multicast(&self, datagram: &[u8]) {
        self.send_sd_unicast(SocketAddrV4::new(SD_GROUP, SD_PORT), datagram);
    }

    /// Sends `datagram` from the peer's SD port to `to`.
    pub fn send_sd_unicast(&self, to: SocketAddrV4, datagram: &[u8]) {
        send(&self.sd, to, datagram);
    }

    /// Sends `datagram` from the peer's service port to `to`.
    pub fn send_unicast(&self, to: SocketAddrV4, datagram: &[u8]) {
        send(&self.uni, to, datagram);
    }

    /// The first SD datagram, multicast or unicast to the peer, for which
    /// `pred` holds, or `None` if none arrives within `timeout`. The peer's
    /// own multicast sends, looped back to it, are skipped.
    pub fn recv_sd(&self, timeout: Duration, pred: impl Fn(&[u8]) -> bool) -> Option<Vec<u8>> {
        let own = SocketAddrV4::new(PEER_IP, SD_PORT);
        let deadline = Instant::now() + timeout;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            for sock in [&self.sd_group, &self.sd] {
                if let Some((datagram, src)) = recv_from(sock, left.min(POLL))
                    && src != own
                    && pred(&datagram)
                {
                    return Some(datagram);
                }
            }
        }
        None
    }

    /// The next datagram on the peer's service port and its source, or
    /// `None` if none arrives within `timeout`.
    pub fn recv_unicast(&self, timeout: Duration) -> Option<(Vec<u8>, SocketAddrV4)> {
        recv_from(&self.uni, timeout)
    }
}

/// A UDP socket bound to `addr` with `SO_REUSEADDR` and `SO_REUSEPORT`, so it
/// can share the SD port with the runtime under test.
fn reusable(addr: SocketAddrV4) -> std::io::Result<socket2::Socket> {
    let s = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, None)?;
    s.set_reuse_address(true)?;
    s.set_reuse_port(true)?;
    s.bind(&addr.into())?;
    Ok(s)
}

fn send(sock: &UdpSocket, to: SocketAddrV4, datagram: &[u8]) {
    let n = sock
        .send_to(datagram, to)
        .unwrap_or_else(|e| panic!("frame peer: sending to {to} failed: {e}"));
    assert_eq!(n, datagram.len(), "frame peer: short send to {to}");
}

fn recv_from(sock: &UdpSocket, timeout: Duration) -> Option<(Vec<u8>, SocketAddrV4)> {
    // A zero read timeout is rejected; it would mean "block forever".
    sock.set_read_timeout(Some(timeout.max(Duration::from_millis(1))))
        .unwrap_or_else(|e| panic!("frame peer: could not set a read timeout: {e}"));
    let mut buf = vec![0; MAX_DATAGRAM];
    match sock.recv_from(&mut buf) {
        Ok((n, SocketAddr::V4(src))) => {
            buf.truncate(n);
            Some((buf, src))
        }
        Ok((_, SocketAddr::V6(src))) => unreachable!("IPv6 source {src} on an IPv4 socket"),
        Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => None,
        Err(e) => panic!("frame peer: receive failed: {e}"),
    }
}

/// Builders for SOME/IP and SOME/IP-SD datagrams. All multi-byte fields are
/// big-endian.
pub mod build {
    use std::net::Ipv4Addr;

    /// A 16-byte SOME/IP header. The length field covers the 8 header bytes
    /// after it plus `payload_len`. Protocol and interface version are both 1
    /// (SD's required values, and the major version of the harness's service).
    pub fn someip_header(
        service: u16,
        method: u16,
        client: u16,
        session: u16,
        msg_type: u8,
        return_code: u8,
        payload_len: usize,
    ) -> Vec<u8> {
        let length = u32::try_from(8 + payload_len).expect("payload too long for SOME/IP");
        let mut h = Vec::with_capacity(16);
        h.extend(service.to_be_bytes());
        h.extend(method.to_be_bytes());
        h.extend(length.to_be_bytes());
        h.extend(client.to_be_bytes());
        h.extend(session.to_be_bytes());
        h.extend([1, 1, msg_type, return_code]);
        h
    }

    /// A complete SOME/IP-SD datagram: the SOME/IP header (message ID
    /// `0xFFFF8100`, client 0, notification) and the SD payload.
    pub fn sd_message(
        session: u16,
        reboot: bool,
        unicast: bool,
        entries: &[[u8; 16]],
        options: &[Vec<u8>],
    ) -> Vec<u8> {
        let entries: Vec<u8> = entries.concat();
        let options: Vec<u8> = options.concat();
        let len = |b: &[u8]| u32::try_from(b.len()).expect("SD array too long");
        let flags = (u8::from(reboot) << 7) | (u8::from(unicast) << 6);

        let mut sd = vec![flags, 0, 0, 0];
        sd.extend(len(&entries).to_be_bytes());
        sd.extend(&entries);
        sd.extend(len(&options).to_be_bytes());
        sd.extend(&options);

        let mut d = someip_header(0xFFFF, 0x8100, 0x0000, session, 0x02, 0x00, sd.len());
        d.extend(sd);
        d
    }

    fn ttl_bytes(ttl: u32) -> [u8; 3] {
        assert!(ttl <= 0x00FF_FFFF, "TTL {ttl:#x} does not fit in 24 bits");
        let [_, a, b, c] = ttl.to_be_bytes();
        [a, b, c]
    }

    fn option_counts(n1: u8, n2: u8) -> u8 {
        assert!(
            n1 <= 0x0F && n2 <= 0x0F,
            "option counts {n1}/{n2} exceed 4 bits"
        );
        (n1 << 4) | n2
    }

    /// A service entry (Find, Offer or StopOffer). `n1` and `n2` are the
    /// option counts of the two runs (4 bits each); `ttl` is 24 bits.
    #[allow(clippy::too_many_arguments)]
    pub fn service_entry(
        entry_type: u8,
        idx1: u8,
        idx2: u8,
        n1: u8,
        n2: u8,
        service: u16,
        instance: u16,
        major: u8,
        ttl: u32,
        minor: u32,
    ) -> [u8; 16] {
        let [s0, s1] = service.to_be_bytes();
        let [i0, i1] = instance.to_be_bytes();
        let [t0, t1, t2] = ttl_bytes(ttl);
        let [m0, m1, m2, m3] = minor.to_be_bytes();
        [
            entry_type,
            idx1,
            idx2,
            option_counts(n1, n2),
            s0,
            s1,
            i0,
            i1,
            major,
            t0,
            t1,
            t2,
            m0,
            m1,
            m2,
            m3,
        ]
    }

    /// An eventgroup entry (Subscribe or SubscribeAck) with one options run.
    /// `counter` is 4 bits; `ttl` is 24 bits.
    #[allow(clippy::too_many_arguments)]
    pub fn eventgroup_entry(
        entry_type: u8,
        idx1: u8,
        n1: u8,
        service: u16,
        instance: u16,
        major: u8,
        ttl: u32,
        initial_data_requested: bool,
        counter: u8,
        eventgroup: u16,
    ) -> [u8; 16] {
        assert!(counter <= 0x0F, "counter {counter} exceeds 4 bits");
        let [s0, s1] = service.to_be_bytes();
        let [i0, i1] = instance.to_be_bytes();
        let [t0, t1, t2] = ttl_bytes(ttl);
        let [g0, g1] = eventgroup.to_be_bytes();
        [
            entry_type,
            idx1,
            0,
            option_counts(n1, 0),
            s0,
            s1,
            i0,
            i1,
            major,
            t0,
            t1,
            t2,
            0,
            (u8::from(initial_data_requested) << 7) | counter,
            g0,
            g1,
        ]
    }

    /// An IPv4 endpoint option (type `0x04`, length 9). `l4` is the IANA
    /// protocol number: `0x11` for UDP, `0x06` for TCP.
    pub fn ipv4_endpoint_option(ip: Ipv4Addr, l4: u8, port: u16) -> Vec<u8> {
        let [a, b, c, d] = ip.octets();
        let [p0, p1] = port.to_be_bytes();
        raw_option(0x04, &[a, b, c, d, 0, l4, p0, p1])
    }

    /// An option of any type: length, type, a reserved byte, then `body`.
    /// The length counts the reserved byte and `body`.
    pub fn raw_option(option_type: u8, body: &[u8]) -> Vec<u8> {
        let length = u16::try_from(body.len() + 1).expect("option body too long");
        let mut o = Vec::with_capacity(4 + body.len());
        o.extend(length.to_be_bytes());
        o.extend([option_type, 0]);
        o.extend(body);
        o
    }
}

/// Readers for captured datagrams. They read only what the datagram holds,
/// so a truncated or malformed datagram yields fewer results, never a panic.
pub mod parse {
    /// The `(type, ttl)` of each entry in an SD datagram, or nothing if the
    /// datagram is not SD.
    pub fn sd_entry_types(datagram: &[u8]) -> Vec<(u8, u32)> {
        let Some(sd) = datagram.strip_prefix(&[0xFF, 0xFF, 0x81, 0x00][..]) else {
            return Vec::new();
        };
        // `sd` starts at the length field: 12 more header bytes, then flags
        // and reserved (4), then the entries length (4).
        let Some(len) = sd.get(16..20) else {
            return Vec::new();
        };
        let len = u32::from_be_bytes([len[0], len[1], len[2], len[3]]) as usize;
        let entries = &sd[20..];
        let entries = &entries[..len.min(entries.len())];
        let (entries, _partial) = entries.as_chunks::<16>();
        entries
            .iter()
            .map(|e| (e[0], u32::from_be_bytes([0, e[9], e[10], e[11]])))
            .collect()
    }
}
