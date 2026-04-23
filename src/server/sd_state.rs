//! Service Discovery session-state tracking, decoupled from socket ownership.
//!
//! [`SdStateManager`] owns the session-ID counter used by every outgoing
//! SOME/IP-SD message this server emits (`OfferService` announcements,
//! unicast Offer replies, `SubscribeAck`, `SubscribeNack`). It also builds
//! and sends `OfferService` announcements when given a socket.
//!
//! Keeping this state in its own type prepares the server for upcoming
//! transport abstraction: once `TransportSocket` lands, the `&UdpSocket`
//! parameter on [`SdStateManager::send_offer_service`] becomes the single
//! migration point for the announcement path.

use core::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::{net::SocketAddrV4, vec::Vec};
use tokio::net::UdpSocket;

use crate::protocol::sd::{
    self, Entry, Flags, OptionsCount, RebootFlag, ServiceEntry, TransportProtocol,
};

use super::{Error, ServerConfig};

/// Tracks the SD session-ID counter and emits `OfferService` announcements.
///
/// Session IDs increment with each SD message and wrap from `0xFFFF` back
/// to `0x0001` (skipping `0`, which is reserved). Per AUTOSAR SOME/IP-SD,
/// the reboot flag on emitted SD messages is
/// [`RebootFlag::RecentlyRebooted`] from startup until the counter wraps
/// once, then [`RebootFlag::Continuous`] permanently — `SdStateManager`
/// tracks that transition and exposes it via [`Self::reboot_flag`] so every
/// server-side SD emission path reads from a single source of truth.
#[derive(Debug)]
pub(super) struct SdStateManager {
    session_id: AtomicU16,
    /// `true` once [`Self::next_session_id`] has advanced past `0xFFFF`.
    /// Monotonic: never transitions back to `false`.
    has_wrapped: AtomicBool,
}

impl SdStateManager {
    pub(super) const fn new() -> Self {
        Self::with_initial(1)
    }

    /// Construct with a specific starting session counter. Primarily used by
    /// tests to validate wrap behavior; callers in production should use
    /// [`Self::new`].
    pub(super) const fn with_initial(initial: u16) -> Self {
        Self {
            session_id: AtomicU16::new(initial),
            has_wrapped: AtomicBool::new(false),
        }
    }

    /// Advance the counter and return the next SOME/IP-SD session ID
    /// (`client_id = 0`, session ID in the low 16 bits). Skips 0 on wrap,
    /// and latches [`Self::has_wrapped`] the first time the counter crosses
    /// the `0xFFFF → 0x0001` boundary so the reboot flag flips to
    /// [`RebootFlag::Continuous`] permanently.
    pub(super) fn next_session_id(&self) -> u32 {
        let prev = self
            .session_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                let next = v.wrapping_add(1);
                Some(if next == 0 { 1 } else { next })
            })
            .unwrap();
        // The only value whose successor wraps through 0 is 0xFFFF; latch
        // the flag exactly on that transition.
        if prev == u16::MAX {
            self.has_wrapped.store(true, Ordering::Relaxed);
        }
        let next = prev.wrapping_add(1);
        u32::from(if next == 0 { 1 } else { next })
    }

    /// Current SD reboot flag for this server.
    ///
    /// Returns [`RebootFlag::RecentlyRebooted`] until the session counter
    /// has wrapped past `0xFFFF` at least once, then
    /// [`RebootFlag::Continuous`] permanently. Every server-side SD
    /// emission path ([`Self::send_offer_service`], plus the unicast
    /// offer / `SubscribeAck` / `SubscribeNack` paths in
    /// [`crate::server::Server`]) calls this so the flag on the wire
    /// reflects a single tracked state.
    pub(super) fn reboot_flag(&self) -> RebootFlag {
        RebootFlag::from(!self.has_wrapped.load(Ordering::Relaxed))
    }

    /// Send a multicast `OfferService` announcement for the given config.
    pub(super) async fn send_offer_service(
        &self,
        config: &ServerConfig,
        socket: &UdpSocket,
    ) -> Result<(), Error> {
        use crate::protocol::Header as SomeIpHeader;
        use crate::traits::WireFormat;

        let entry = Entry::OfferService(ServiceEntry {
            index_first_options_run: 0,
            index_second_options_run: 0,
            options_count: OptionsCount::new(1, 0),
            service_id: config.service_id,
            instance_id: config.instance_id,
            major_version: config.major_version,
            ttl: config.ttl,
            minor_version: config.minor_version,
        });

        let option = sd::Options::IpV4Endpoint {
            ip: config.interface,
            port: config.local_port,
            protocol: TransportProtocol::Udp,
        };

        let entries = [entry];
        let options = [option];
        // Advance the session counter FIRST so `has_wrapped` latches on
        // the wrap transition, then derive the reboot flag for this
        // same message. Without this ordering the message carrying
        // session_id=0x0001 after a wrap would still advertise
        // `RebootFlag::RecentlyRebooted`, and the flip would only land
        // on the NEXT emission — violating AUTOSAR SOME/IP-SD semantics
        // (the wrap message itself should carry `Continuous`).
        let sid = self.next_session_id();
        let sd_payload = sd::Header::new(Flags::new_sd(self.reboot_flag()), &entries, &options);

        let mut sd_data = Vec::new();
        sd_payload.encode(&mut sd_data)?;

        let someip_header = SomeIpHeader::new_sd(sid, sd_data.len());

        let mut buffer = Vec::new();
        someip_header.encode(&mut buffer)?;
        buffer.extend_from_slice(&sd_data);

        let multicast_addr = SocketAddrV4::new(sd::MULTICAST_IP, sd::MULTICAST_PORT);

        tracing::trace!(
            "Sending OfferService: service=0x{:04X}, instance={}, port={}, size={} bytes",
            config.service_id,
            config.instance_id,
            config.local_port,
            buffer.len()
        );
        tracing::trace!(
            "OfferService data: {:02X?}",
            &buffer[..buffer.len().min(64)]
        );

        socket.send_to(&buffer, multicast_addr).await?;
        tracing::trace!("Sent to {}", multicast_addr);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{SdStateManager, ServerConfig};
    use crate::protocol::sd::{self, EntryType, Flags, RebootFlag, TransportProtocol};
    use crate::protocol::{MessageType, MessageView, ReturnCode};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::time::Duration;
    use tokio::net::UdpSocket;

    /// Test-only `service_id` for `send_offer_service` tests. Distinct from
    /// the 0x5B / 0x5C values used elsewhere in this crate so that parallel
    /// tests joined to the same SD multicast group do not produce false
    /// matches. If you add a new test that emits a multicast `OfferService`,
    /// give it its own dedicated `service_id` too.
    const TEST_SERVICE_ID: u16 = 0xFE01;
    const TEST_INSTANCE_ID: u16 = 0x42;
    /// Port value placed in the emitted `IpV4Endpoint` option so the
    /// round-trip assertion has something non-zero to check. The test does
    /// not bind this port — it only appears in the announcement payload.
    const TEST_ADVERTISED_PORT: u16 = 40210;

    #[test]
    fn next_session_id_wraps_past_ffff_skipping_zero() {
        let sd = SdStateManager::with_initial(0xFFFE);

        // 0xFFFE -> 0xFFFF
        assert_eq!(sd.next_session_id(), 0xFFFF);

        // 0xFFFF -> wraps to 0x0001 (0 is skipped)
        assert_eq!(sd.next_session_id(), 0x0001);
    }

    #[test]
    fn next_session_id_starts_at_two_from_default_new() {
        let sd = SdStateManager::new();
        // new() seeds at 1; first next_session_id increments to 2
        assert_eq!(sd.next_session_id(), 2);
    }

    // ── Reboot-flag tracking ────────────────────────────────────────────
    //
    // AUTOSAR SOME/IP-SD: the reboot bit on emitted SD messages must be
    // set until the session counter wraps past `0xFFFF` for the first
    // time, then cleared permanently. These tests drive `SdStateManager`
    // directly (no socket) and verify the state machine that every
    // server-side SD emission path (`send_offer_service`, plus unicast
    // offer / `SubscribeAck` / `SubscribeNack` in `server::Server`) now
    // reads from via [`SdStateManager::reboot_flag`].

    #[test]
    fn reboot_flag_is_recently_rebooted_on_fresh_manager() {
        // Default constructor: counter hasn't wrapped, flag must indicate
        // a recent reboot so peers can re-synchronize SD state.
        let sd = SdStateManager::new();
        assert_eq!(sd.reboot_flag(), RebootFlag::RecentlyRebooted);
    }

    #[test]
    fn reboot_flag_stays_recently_rebooted_below_wrap() {
        // Advancing the counter short of a wrap must not flip the flag —
        // it's specifically the 0xFFFF → 0x0001 transition that matters,
        // not "has next_session_id been called more than once".
        let sd = SdStateManager::with_initial(0x1233);
        for _ in 0..10 {
            sd.next_session_id();
        }
        assert_eq!(sd.reboot_flag(), RebootFlag::RecentlyRebooted);
    }

    #[test]
    fn reboot_flag_flips_to_continuous_exactly_on_wrap() {
        // Step the counter across the wrap boundary and assert the flag
        // transitions on the precise call that crosses 0xFFFF → 0x0001.
        let sd = SdStateManager::with_initial(0xFFFE);
        assert_eq!(sd.reboot_flag(), RebootFlag::RecentlyRebooted);

        // 0xFFFE -> 0xFFFF: prev=0xFFFE, no wrap.
        assert_eq!(sd.next_session_id(), 0xFFFF);
        assert_eq!(
            sd.reboot_flag(),
            RebootFlag::RecentlyRebooted,
            "counter reached 0xFFFF but has not yet wrapped — flag must still be RecentlyRebooted",
        );

        // 0xFFFF -> 0x0001 (skip 0): prev=0xFFFF, wrap latches.
        assert_eq!(sd.next_session_id(), 0x0001);
        assert_eq!(
            sd.reboot_flag(),
            RebootFlag::Continuous,
            "wrap just occurred — flag must now be Continuous",
        );
    }

    #[test]
    fn reboot_flag_is_monotonic_after_wrap() {
        // Once the flag latches to Continuous it never goes back, even
        // after the counter wraps a second time or is advanced
        // indefinitely. Guard against a regression that would re-derive
        // the flag from the current counter value (which would wrongly
        // flip back to RecentlyRebooted at 0x0001).
        let sd = SdStateManager::with_initial(0xFFFE);
        sd.next_session_id(); // -> 0xFFFF
        sd.next_session_id(); // wrap -> 0x0001
        assert_eq!(sd.reboot_flag(), RebootFlag::Continuous);

        // Many further advances, including crossing 0xFFFF again.
        for _ in 0..(u32::from(u16::MAX) + 5) {
            sd.next_session_id();
        }
        assert_eq!(sd.reboot_flag(), RebootFlag::Continuous);
    }

    // ── Multicast-loopback harness ──────────────────────────────────────
    //
    // All tests below drive `send_offer_service` against a real UDP socket
    // and read the emitted packet off a second socket joined to the SD
    // multicast group. These are `#[ignore]`d on environments whose
    // loopback interface does not carry the `MULTICAST` flag (check with
    // `ip link show lo`); on such hosts the kernel drops multicast on
    // `lo` before loopback reflection, so the receiver times out. Runs
    // in any environment where loopback multicast is available.

    /// Bind a receiver socket on the SD multicast port, ready to
    /// `join_multicast_v4`.
    fn build_mcast_receiver(interface: Ipv4Addr) -> std::io::Result<UdpSocket> {
        let raw = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        raw.set_reuse_address(true)?;
        #[cfg(unix)]
        raw.set_reuse_port(true)?;
        raw.set_multicast_loop_v4(true)?;
        raw.bind(&SocketAddr::new(IpAddr::V4(interface), sd::MULTICAST_PORT).into())?;
        raw.set_nonblocking(true)?;
        UdpSocket::from_std(raw.into())
    }

    /// Bind a sender socket on an ephemeral port with `multicast_if` pinned
    /// to the loopback interface so emitted packets loop back to any
    /// receiver joined to the same group on that interface.
    fn build_mcast_sender(interface: Ipv4Addr) -> std::io::Result<UdpSocket> {
        let raw = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        raw.set_reuse_address(true)?;
        #[cfg(unix)]
        raw.set_reuse_port(true)?;
        raw.set_multicast_loop_v4(true)?;
        raw.set_multicast_if_v4(&interface)?;
        raw.bind(&SocketAddr::new(IpAddr::V4(interface), 0).into())?;
        raw.set_nonblocking(true)?;
        UdpSocket::from_std(raw.into())
    }

    /// Fields extracted from a received SOME/IP-SD `OfferService` packet.
    /// Keeping these together makes per-test assertions a straight list of
    /// `assert_eq!`s against expected values.
    struct ReceivedOffer {
        request_id: u32,
        someip_service_id: u16,
        someip_method_id: u16,
        message_type: MessageType,
        return_code: ReturnCode,
        protocol_version: u8,
        interface_version: u8,
        flags: Flags,
        entry_service_id: u16,
        entry_instance_id: u16,
        entry_major_version: u8,
        entry_minor_version: u32,
        entry_ttl: u32,
        endpoint_ip: Ipv4Addr,
        endpoint_port: u16,
        endpoint_protocol: TransportProtocol,
    }

    /// Wait for a multicast `OfferService` matching `expected_service_id`,
    /// returning its decoded fields. Other packets on the group (from
    /// concurrent tests) are ignored; a single outer timeout bounds the
    /// whole filter loop.
    async fn recv_our_offer(
        rx: &UdpSocket,
        expected_service_id: u16,
        within: Duration,
    ) -> ReceivedOffer {
        let recv_loop = async {
            let mut buf = [0u8; 2048];
            loop {
                let (len, _from) = rx
                    .recv_from(&mut buf)
                    .await
                    .expect("recv_from should succeed");
                let Ok(view) = MessageView::parse(&buf[..len]) else {
                    continue;
                };
                if view.header().message_id().service_id() != 0xFFFF {
                    continue;
                }
                let Ok(sd_view) = view.sd_header() else {
                    continue;
                };
                let Some(entry) = sd_view.entries().next() else {
                    continue;
                };
                if !matches!(entry.entry_type(), Ok(EntryType::OfferService)) {
                    continue;
                }
                if entry.service_id() != expected_service_id {
                    continue;
                }
                let first_option = sd_view
                    .options()
                    .next()
                    .expect("OfferService should carry an endpoint option");
                let (endpoint_ip, endpoint_protocol, endpoint_port) = first_option
                    .as_ipv4()
                    .expect("endpoint option should decode as IPv4");
                return ReceivedOffer {
                    request_id: view.header().request_id(),
                    someip_service_id: view.header().message_id().service_id(),
                    someip_method_id: view.header().message_id().method_id(),
                    message_type: view.header().message_type().message_type(),
                    return_code: view.header().return_code(),
                    protocol_version: view.header().protocol_version(),
                    interface_version: view.header().interface_version(),
                    flags: sd_view.flags(),
                    entry_service_id: entry.service_id(),
                    entry_instance_id: entry.instance_id(),
                    entry_major_version: entry.major_version(),
                    entry_minor_version: entry.minor_version(),
                    entry_ttl: entry.ttl(),
                    endpoint_ip,
                    endpoint_port,
                    endpoint_protocol,
                };
            }
        };
        tokio::time::timeout(within, recv_loop)
            .await
            .expect("timed out waiting for our OfferService")
    }

    /// Assert every field of the SOME/IP + SD envelope that
    /// `send_offer_service` is responsible for — not just the entry body.
    /// A future regression that garbles the endpoint option, flips a flag,
    /// or changes the SOME/IP message type should fail here.
    ///
    /// `expected_reboot` lets pre-wrap callers assert `RecentlyRebooted`
    /// and post-wrap callers assert `Continuous`; the flag is tracked by
    /// `SdStateManager::has_wrapped` and read via `reboot_flag()` at each
    /// send.
    fn assert_offer_matches(
        offer: &ReceivedOffer,
        config: &ServerConfig,
        expected_request_id: u32,
        expected_reboot: RebootFlag,
    ) {
        // SOME/IP envelope
        assert_eq!(offer.someip_service_id, 0xFFFF, "SD uses service_id 0xFFFF");
        assert_eq!(offer.someip_method_id, 0x8100, "SD uses method_id 0x8100");
        assert_eq!(offer.message_type, MessageType::Notification);
        assert_eq!(offer.return_code, ReturnCode::Ok);
        assert_eq!(offer.protocol_version, 0x01);
        assert_eq!(offer.interface_version, 0x01);
        assert_eq!(
            offer.request_id, expected_request_id,
            "request_id is session_id in low 16 bits, client_id zero in high 16",
        );
        // SD flags — reboot comes from SdStateManager::reboot_flag (latches
        // to Continuous after the session counter wraps past 0xFFFF);
        // unicast is always true for SD.
        assert_eq!(offer.flags.reboot(), expected_reboot);
        assert!(offer.flags.unicast());
        // OfferService entry
        assert_eq!(offer.entry_service_id, config.service_id);
        assert_eq!(offer.entry_instance_id, config.instance_id);
        assert_eq!(offer.entry_major_version, config.major_version);
        assert_eq!(offer.entry_minor_version, config.minor_version);
        assert_eq!(offer.entry_ttl, config.ttl);
        // Endpoint option
        assert_eq!(offer.endpoint_ip, config.interface);
        assert_eq!(offer.endpoint_port, config.local_port);
        assert_eq!(offer.endpoint_protocol, TransportProtocol::Udp);
    }

    /// Standard loopback receiver/sender pair used by the send-path tests.
    fn mcast_rx_tx() -> (UdpSocket, UdpSocket) {
        let interface = Ipv4Addr::LOCALHOST;
        let rx = build_mcast_receiver(interface).expect("bind receiver");
        rx.join_multicast_v4(sd::MULTICAST_IP, interface)
            .expect("join SD multicast group");
        let tx = build_mcast_sender(interface).expect("bind sender");
        (rx, tx)
    }

    #[ignore = "requires MULTICAST on loopback; skipped on hosts whose `lo` \
                lacks the MULTICAST flag. Runs in any environment where \
                loopback multicast is available."]
    #[tokio::test]
    async fn send_offer_service_emits_parseable_offer_to_multicast() {
        let config = ServerConfig::new(
            Ipv4Addr::LOCALHOST,
            TEST_ADVERTISED_PORT,
            TEST_SERVICE_ID,
            TEST_INSTANCE_ID,
        );
        let (rx, tx) = mcast_rx_tx();

        // Seed with a recognisable value so on-wire session_id is exact.
        let sd_state = SdStateManager::with_initial(0x1233);
        sd_state
            .send_offer_service(&config, &tx)
            .await
            .expect("send_offer_service should succeed on a configured socket");

        let offer = recv_our_offer(&rx, config.service_id, Duration::from_secs(2)).await;
        // next_session_id advances 0x1233 -> 0x1234; client_id is zero.
        // Fresh SdStateManager: counter has not wrapped, reboot flag is
        // RecentlyRebooted.
        assert_offer_matches(&offer, &config, 0x0000_1234, RebootFlag::RecentlyRebooted);
    }

    #[ignore = "requires MULTICAST on loopback; skipped on hosts whose `lo` \
                lacks the MULTICAST flag. Runs in any environment where \
                loopback multicast is available."]
    #[tokio::test]
    async fn send_offer_service_advances_session_id_across_calls() {
        // Back-to-back sends must consume distinct, incrementing session
        // IDs — catches a regression where `send_offer_service` reads the
        // counter without advancing it, or reuses a cached value.
        let config = ServerConfig::new(
            Ipv4Addr::LOCALHOST,
            TEST_ADVERTISED_PORT,
            TEST_SERVICE_ID,
            TEST_INSTANCE_ID,
        );
        let (rx, tx) = mcast_rx_tx();

        let sd_state = SdStateManager::with_initial(0x1233);
        sd_state.send_offer_service(&config, &tx).await.unwrap();
        sd_state.send_offer_service(&config, &tx).await.unwrap();

        let first = recv_our_offer(&rx, config.service_id, Duration::from_secs(2)).await;
        let second = recv_our_offer(&rx, config.service_id, Duration::from_secs(2)).await;
        assert_eq!(first.request_id, 0x0000_1234);
        assert_eq!(second.request_id, 0x0000_1235);
    }

    #[ignore = "requires MULTICAST on loopback; skipped on hosts whose `lo` \
                lacks the MULTICAST flag. Runs in any environment where \
                loopback multicast is available."]
    #[tokio::test]
    async fn send_offer_service_wraps_session_id_through_zero_on_send() {
        // Session counter wrap must be visible on the wire: 0xFFFE -> 0xFFFF
        // -> 0x0001 (skipping the reserved 0). Exercises the wrap branch
        // *through* the send path, not only the unit test of next_session_id.
        let config = ServerConfig::new(
            Ipv4Addr::LOCALHOST,
            TEST_ADVERTISED_PORT,
            TEST_SERVICE_ID,
            TEST_INSTANCE_ID,
        );
        let (rx, tx) = mcast_rx_tx();

        let sd_state = SdStateManager::with_initial(0xFFFE);
        sd_state.send_offer_service(&config, &tx).await.unwrap();
        sd_state.send_offer_service(&config, &tx).await.unwrap();

        let first = recv_our_offer(&rx, config.service_id, Duration::from_secs(2)).await;
        let second = recv_our_offer(&rx, config.service_id, Duration::from_secs(2)).await;
        assert_eq!(first.request_id, 0x0000_FFFF);
        assert_eq!(
            second.request_id, 0x0000_0001,
            "must skip reserved 0 on wrap"
        );
        // Reboot flag latches: the first emission goes out before the
        // wrap happens (prev=0xFFFE), so it still advertises
        // RecentlyRebooted; the second emission is the one whose
        // next_session_id call crossed 0xFFFF -> 0x0001, so the flag
        // Flips to Continuous permanently from there on.
        assert_eq!(
            first.flags.reboot(),
            RebootFlag::RecentlyRebooted,
            "first emit is pre-wrap and must still advertise RecentlyRebooted",
        );
        assert_eq!(
            second.flags.reboot(),
            RebootFlag::Continuous,
            "post-wrap emit must advertise Continuous",
        );
    }

    #[ignore = "requires MULTICAST on loopback; skipped on hosts whose `lo` \
                lacks the MULTICAST flag. Runs in any environment where \
                loopback multicast is available."]
    #[tokio::test]
    async fn send_offer_service_preserves_zero_ttl() {
        // TTL=0 is a legitimate SOME/IP-SD value meaning "stop offering";
        // `send_offer_service` must preserve it end-to-end rather than,
        // say, defaulting it back to the ServerConfig::new value of 3.
        let mut config = ServerConfig::new(
            Ipv4Addr::LOCALHOST,
            TEST_ADVERTISED_PORT,
            TEST_SERVICE_ID,
            TEST_INSTANCE_ID,
        );
        config.ttl = 0;
        let (rx, tx) = mcast_rx_tx();

        let sd_state = SdStateManager::with_initial(0x1233);
        sd_state.send_offer_service(&config, &tx).await.unwrap();

        let offer = recv_our_offer(&rx, config.service_id, Duration::from_secs(2)).await;
        assert_offer_matches(&offer, &config, 0x0000_1234, RebootFlag::RecentlyRebooted);
        // Belt-and-suspenders: assert_offer_matches already checks this,
        // but the purpose of this test is specifically the zero case.
        assert_eq!(offer.entry_ttl, 0);
    }
}
