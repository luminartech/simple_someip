//! Service Discovery session-state tracking, decoupled from socket ownership.
//!
//! [`SdStateManager`] owns the session ID and reboot flag of every
//! SOME/IP-SD message this server sends: `OfferService` (multicast and
//! unicast), `FindService`, `SubscribeEventgroup`, `SubscribeAck` and
//! `SubscribeNack`. It also builds the server's `OfferService`.

use crate::protocol::sd::RebootFlag;
use crate::sd_codec::{SdSession, SdSessionCounter};
use crate::transport::TransportSocket;

use super::{Error, ServerConfig};

/// Numbers every SD message the server sends, multicast and unicast, from
/// one [`SdSessionCounter`], and emits its `OfferService` announcements.
#[derive(Debug)]
pub struct SdStateManager {
    session: SdSessionCounter,
}

impl SdStateManager {
    /// Construct an `SdStateManager` whose first SD message gets session
    /// ID 1 and the reboot flag `RecentlyRebooted`.
    ///
    /// `const fn` so consumers can declare a `static`-storage instance
    /// without an allocator:
    ///
    /// ```ignore
    /// static SD_STATE: SdStateManager = SdStateManager::new();
    /// // pass `&SD_STATE` (an `&'static SdStateManager`) into the
    /// // appropriate `Server` constructor.
    /// ```
    #[must_use]
    pub const fn new() -> Self {
        Self::with_initial(0)
    }
}

impl Default for SdStateManager {
    /// Equivalent to [`Self::new`]. Provided for clippy-pedantic
    /// completeness; bare-metal callers should prefer the explicit
    /// `SdStateManager::new()` because it is `const` and works in a
    /// `static` initializer.
    fn default() -> Self {
        Self::new()
    }
}

impl SdStateManager {
    /// Construct as if `initial` was the session ID last sent, before the
    /// counter first wrapped; see [`SdSessionCounter::resume`].
    #[must_use]
    pub const fn with_initial(initial: u16) -> Self {
        Self {
            session: SdSessionCounter::resume(initial, RebootFlag::RecentlyRebooted),
        }
    }

    /// The session of the next SD message: its ID and reboot flag, for the
    /// [`sd_codec`](crate::sd_codec) builders.
    pub fn next_session(&self) -> SdSession {
        self.session.next()
    }

    /// The next SD session ID and the reboot flag of the same message.
    #[cfg(test)]
    pub(crate) fn next_session_id_with_reboot_flag(&self) -> (u32, RebootFlag) {
        let session = self.next_session();
        (u32::from(session.id()), session.reboot())
    }

    /// Advance the counter and return only the session id.
    #[cfg(test)]
    pub(super) fn next_session_id(&self) -> u32 {
        self.next_session_id_with_reboot_flag().0
    }

    /// The reboot flag the counter is at.
    #[cfg(test)]
    pub(super) fn reboot_flag(&self) -> RebootFlag {
        if self.session.has_wrapped() {
            RebootFlag::Continuous
        } else {
            RebootFlag::RecentlyRebooted
        }
    }

    /// Encode `config`'s `OfferService` into `buf` as the next SD message
    /// of this manager; returns its length.
    pub(super) fn encode_offer(
        &self,
        buf: &mut [u8],
        config: &ServerConfig,
    ) -> Result<usize, Error> {
        crate::sd_codec::build_offer_service_datagram(
            buf,
            &config.offer_request(),
            self.next_session(),
        )
        .map_err(super::build_error)
    }

    /// Send a multicast `OfferService` announcement for the given config.
    ///
    /// `buf` is a caller-provided scratch buffer used for encoding the
    /// outgoing frame. Returns [`Error::Capacity`]`("udp_buffer")` if the
    /// encoded frame does not fit in `buf`, a protocol error if
    /// `config.ttl` is outside 1..=[`MAX_TTL`](crate::protocol::sd::MAX_TTL), or the
    /// socket's transport error.
    pub(super) async fn send_offer_service<T: TransportSocket>(
        &self,
        buf: &mut [u8],
        config: &ServerConfig,
        socket: &T,
    ) -> Result<(), Error> {
        let total_len = self.encode_offer(buf, config)?;
        let multicast_addr = config.sd_multicast;

        crate::log::trace!(
            "Sending OfferService: service=0x{:04X}, instance={}, port={}, size={} bytes",
            config.service_id,
            config.instance_id,
            config.local_port,
            total_len
        );
        crate::log::trace!("OfferService data: {:02X?}", &buf[..total_len.min(64)]);

        socket.send_to(&buf[..total_len], multicast_addr).await?;
        crate::log::trace!("Sent to {}", multicast_addr);

        Ok(())
    }
}

// `SdStateHandle` / `WrappableSdStateHandle` were collapsed into the
// unified `crate::transport::SharedHandle<SdStateManager>` /
// `WrappableSharedHandle<SdStateManager>` traits. The blanket impls
// there cover both `&'static SdStateManager` and
// `Arc<SdStateManager>`; no dedicated trait survives here.

#[cfg(all(test, feature = "server-tokio"))]
mod tests {
    use super::{Error, SdStateManager, ServerConfig};
    use crate::protocol::sd::{self, EntryType, Flags, RebootFlag, TransportProtocol};
    use crate::protocol::{MessageType, MessageView, ReturnCode};
    use crate::tokio_transport::TokioSocket;
    use crate::transport::{SocketOptions, TransportFactory};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
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
    fn the_first_session_id_from_new_is_one() {
        let sd = SdStateManager::new();
        assert_eq!(
            sd.next_session_id_with_reboot_flag(),
            (1, RebootFlag::RecentlyRebooted)
        );
        assert_eq!(sd.next_session_id(), 2);
    }

    #[test]
    fn the_session_after_0xffff_is_one_with_the_reboot_flag_cleared() {
        let sd = SdStateManager::with_initial(0xFFFF);
        assert_eq!(
            sd.next_session_id_with_reboot_flag(),
            (1, RebootFlag::Continuous)
        );
    }

    /// Concurrent emitters around the wrap boundary must never produce
    /// a `(session_id, reboot_flag)` pair where one is pre-wrap and the
    /// other is post-wrap. Regression for the two-atomic TOCTOU race.
    ///
    /// We seed near the wrap and have many threads call
    /// `next_session_id_with_reboot_flag` concurrently. Every
    /// `(0xFFFF, _)` must carry `RecentlyRebooted`, every `(0x0001, _)`
    /// (the wrap message) and beyond must carry `Continuous`.
    #[test]
    fn next_session_id_with_reboot_flag_never_mismatches_around_wrap() {
        use std::sync::Arc;
        for _trial in 0..20 {
            let sd = Arc::new(SdStateManager::with_initial(0xFFF0));
            let mut handles = std::vec::Vec::new();
            for _ in 0..32 {
                let s = Arc::clone(&sd);
                handles.push(std::thread::spawn(move || {
                    let (sid, flag) = s.next_session_id_with_reboot_flag();
                    (sid, flag)
                }));
            }
            for h in handles {
                let (sid, flag) = h.join().unwrap();
                // sid is u32 in 1..=0xFFFF (never 0).
                assert!((1..=0xFFFF).contains(&sid), "sid out of range: {sid:#x}");
                if sid == 0xFFFF {
                    // The 0xFFFF emission is the LAST pre-wrap.
                    assert_eq!(
                        flag,
                        RebootFlag::RecentlyRebooted,
                        "sid=0xFFFF must carry RecentlyRebooted"
                    );
                } else if sid <= 0xFFEF {
                    // We seeded at 0xFFF0, so any sid in 1..=0xFFEF
                    // means the counter wrapped past 0xFFFF. Must be
                    // Continuous.
                    assert_eq!(
                        flag,
                        RebootFlag::Continuous,
                        "post-wrap sid={sid:#x} must carry Continuous"
                    );
                }
                // sids in 0xFFF0..=0xFFFE are the pre-wrap window —
                // both flags are valid depending on whether this trial
                // wrapped before/after the emission. Don't assert.
            }
        }
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

    // ── Mock-socket tests (default `cargo test` runs these) ────────────
    //
    // These exercise `send_offer_service`'s encoding + framing path
    // through a mock `TransportSocket` that captures the bytes the
    // loop would have emitted. They run in default `cargo test` (no
    // MULTICAST flag required) and provide the primary coverage for
    // the encoding lines.
    //
    // The `#[ignore]`d multicast tests further down test the SAME
    // properties (session-id advancement, wrap)
    // through a real kernel-loopback multicast socket. Those tests
    // remain because they additionally exercise the
    // `socket.send_to(multicast_addr, ...)` kernel path, which the
    // mock can't observe. Don't delete them in favour of the mocks
    // — the kernel-multicast verification is the only signal we
    // have for "the wire form actually leaves the OS correctly."

    use crate::transport::{IoErrorKind, ReceivedDatagram, TransportError, TransportSocket};
    use std::sync::Mutex;
    use std::vec::Vec;

    /// `TransportSocket` impl that captures every `send_to` call
    /// instead of touching a real network. `recv_from` and the
    /// socket-level queries are stubbed because `send_offer_service`
    /// never touches them.
    struct CapturingSocket {
        sent: Mutex<Vec<(SocketAddrV4, Vec<u8>)>>,
    }

    impl CapturingSocket {
        fn new() -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
            }
        }

        fn drain_sent(&self) -> Vec<(SocketAddrV4, Vec<u8>)> {
            std::mem::take(&mut *self.sent.lock().unwrap())
        }
    }

    impl TransportSocket for CapturingSocket {
        type SendFuture<'a> = core::future::Ready<Result<(), TransportError>>;
        type RecvFuture<'a> = core::future::Pending<Result<ReceivedDatagram, TransportError>>;

        fn send_to<'a>(&'a self, buf: &'a [u8], target: SocketAddrV4) -> Self::SendFuture<'a> {
            self.sent.lock().unwrap().push((target, buf.to_vec()));
            core::future::ready(Ok(()))
        }

        fn recv_from<'a>(&'a self, _buf: &'a mut [u8]) -> Self::RecvFuture<'a> {
            core::future::pending()
        }

        fn local_addr(&self) -> Result<SocketAddrV4, TransportError> {
            Err(TransportError::Io(IoErrorKind::Other))
        }

        fn join_multicast_v4(
            &self,
            _group: Ipv4Addr,
            _iface: Ipv4Addr,
        ) -> Result<(), TransportError> {
            Ok(())
        }

        fn leave_multicast_v4(
            &self,
            _group: Ipv4Addr,
            _iface: Ipv4Addr,
        ) -> Result<(), TransportError> {
            Ok(())
        }
    }

    /// `TransportSocket` that fails on `send_to` so we can test
    /// `send_offer_service`'s error propagation path.
    struct FailingSocket;

    impl TransportSocket for FailingSocket {
        type SendFuture<'a> = core::future::Ready<Result<(), TransportError>>;
        type RecvFuture<'a> = core::future::Pending<Result<ReceivedDatagram, TransportError>>;

        fn send_to<'a>(&'a self, _buf: &'a [u8], _target: SocketAddrV4) -> Self::SendFuture<'a> {
            core::future::ready(Err(TransportError::Io(IoErrorKind::NetworkUnreachable)))
        }

        fn recv_from<'a>(&'a self, _buf: &'a mut [u8]) -> Self::RecvFuture<'a> {
            core::future::pending()
        }

        fn local_addr(&self) -> Result<SocketAddrV4, TransportError> {
            Err(TransportError::Io(IoErrorKind::Other))
        }

        fn join_multicast_v4(
            &self,
            _group: Ipv4Addr,
            _iface: Ipv4Addr,
        ) -> Result<(), TransportError> {
            Ok(())
        }

        fn leave_multicast_v4(
            &self,
            _group: Ipv4Addr,
            _iface: Ipv4Addr,
        ) -> Result<(), TransportError> {
            Ok(())
        }
    }

    /// Assert that the captured datagram is byte-for-byte the
    /// `OfferService` we expected — SOME/IP envelope, SD entry, and
    /// the IPv4 endpoint option.
    fn assert_captured_offer_matches(
        bytes: &[u8],
        target: SocketAddrV4,
        config: &ServerConfig,
        expected_session_id: u32,
        expected_reboot: RebootFlag,
    ) {
        // Goes to the SD multicast group on port 30490.
        assert_eq!(
            target,
            SocketAddrV4::new(sd::MULTICAST_IP, sd::MULTICAST_PORT),
        );
        let view = MessageView::parse(bytes).expect("parses as SOME/IP");
        // SD envelope. message_id = (0xFFFF, 0x8100), notification, ok.
        assert_eq!(view.header().message_id().service_id(), 0xFFFF);
        assert_eq!(view.header().message_id().method_id(), 0x8100);
        assert_eq!(view.header().request_id(), expected_session_id);
        assert_eq!(
            view.header().message_type().message_type(),
            MessageType::Notification,
        );
        assert_eq!(view.header().return_code(), ReturnCode::Ok);
        // SD body.
        let sd_view = view.sd_header().expect("sd header parses");
        assert_eq!(sd_view.flags().reboot(), expected_reboot);
        assert!(sd_view.flags().unicast(), "unicast flag must be set");
        // Exactly one OfferService entry, exactly one IPv4 endpoint.
        assert_eq!(sd_view.entries().count(), 1);
        assert_eq!(sd_view.options().count(), 1);
        let entry = sd_view.entries().next().unwrap();
        assert!(matches!(entry.entry_type(), Ok(EntryType::OfferService),));
        assert_eq!(entry.service_id(), config.service_id);
        assert_eq!(entry.instance_id(), config.instance_id);
        assert_eq!(entry.major_version(), config.major_version);
        assert_eq!(entry.ttl(), config.ttl);
        assert_eq!(entry.minor_version(), config.minor_version);
        let opts_count = entry.options_count();
        assert_eq!(opts_count.first_options_count, 1);
        assert_eq!(opts_count.second_options_count, 0);
        let option = sd_view.options().next().unwrap();
        let (ip, protocol, port) = option.as_ipv4().expect("ipv4 endpoint option");
        assert_eq!(ip, config.interface);
        assert_eq!(port, config.local_port);
        assert_eq!(protocol, TransportProtocol::Udp);
    }

    #[tokio::test]
    async fn send_offer_service_through_mock_emits_full_someip_sd_envelope() {
        let config = ServerConfig::new(TEST_SERVICE_ID, TEST_INSTANCE_ID)
            .with_interface(Ipv4Addr::LOCALHOST)
            .with_local_port(TEST_ADVERTISED_PORT);
        let sd_state = SdStateManager::with_initial(0x1233);
        let sock = CapturingSocket::new();

        sd_state
            .send_offer_service(&mut [0u8; crate::UDP_BUFFER_SIZE], &config, &sock)
            .await
            .expect("send_offer_service should succeed against the mock");

        let captured = sock.drain_sent();
        assert_eq!(captured.len(), 1, "expected exactly one send_to call");
        let (target, bytes) = &captured[0];
        // Initial counter 0x1233 → first emission carries 0x0000_1234,
        // and the manager has not wrapped, so reboot=RecentlyRebooted.
        assert_captured_offer_matches(
            bytes,
            *target,
            &config,
            0x0000_1234,
            RebootFlag::RecentlyRebooted,
        );
    }

    #[tokio::test]
    async fn send_offer_service_through_mock_advances_session_id_across_calls() {
        let config = ServerConfig::new(TEST_SERVICE_ID, TEST_INSTANCE_ID)
            .with_interface(Ipv4Addr::LOCALHOST)
            .with_local_port(TEST_ADVERTISED_PORT);
        let sd_state = SdStateManager::with_initial(0x1233);
        let sock = CapturingSocket::new();

        sd_state
            .send_offer_service(&mut [0u8; crate::UDP_BUFFER_SIZE], &config, &sock)
            .await
            .unwrap();
        sd_state
            .send_offer_service(&mut [0u8; crate::UDP_BUFFER_SIZE], &config, &sock)
            .await
            .unwrap();

        let sent = sock.drain_sent();
        assert_eq!(sent.len(), 2);
        let v0 = MessageView::parse(&sent[0].1).unwrap();
        let v1 = MessageView::parse(&sent[1].1).unwrap();
        assert_eq!(v0.header().request_id(), 0x0000_1234);
        assert_eq!(v1.header().request_id(), 0x0000_1235);
    }

    #[tokio::test]
    async fn send_offer_service_through_mock_reboot_flag_flips_on_wrap() {
        let config = ServerConfig::new(TEST_SERVICE_ID, TEST_INSTANCE_ID)
            .with_interface(Ipv4Addr::LOCALHOST)
            .with_local_port(TEST_ADVERTISED_PORT);
        // Seed so the FIRST send takes 0xFFFE → 0xFFFF (still
        // RecentlyRebooted) and the SECOND sees the wrap to 0x0001
        // (Continuous).
        let sd_state = SdStateManager::with_initial(0xFFFE);
        let sock = CapturingSocket::new();
        sd_state
            .send_offer_service(&mut [0u8; crate::UDP_BUFFER_SIZE], &config, &sock)
            .await
            .unwrap();
        sd_state
            .send_offer_service(&mut [0u8; crate::UDP_BUFFER_SIZE], &config, &sock)
            .await
            .unwrap();

        let sent = sock.drain_sent();
        assert_eq!(sent.len(), 2);
        let v0 = MessageView::parse(&sent[0].1).unwrap();
        let v1 = MessageView::parse(&sent[1].1).unwrap();
        assert_eq!(v0.header().request_id(), 0x0000_FFFF);
        assert_eq!(
            v1.header().request_id(),
            0x0000_0001,
            "must skip reserved 0 on wrap",
        );
        assert_eq!(
            v0.sd_header().unwrap().flags().reboot(),
            RebootFlag::RecentlyRebooted,
            "first emit is pre-wrap",
        );
        assert_eq!(
            v1.sd_header().unwrap().flags().reboot(),
            RebootFlag::Continuous,
            "second emit is post-wrap",
        );
    }

    #[tokio::test]
    async fn send_offer_service_rejects_zero_ttl_without_sending() {
        let mut config = ServerConfig::new(TEST_SERVICE_ID, TEST_INSTANCE_ID)
            .with_interface(Ipv4Addr::LOCALHOST)
            .with_local_port(TEST_ADVERTISED_PORT);
        config.ttl = 0;
        let sd_state = SdStateManager::with_initial(0x1233);
        let sock = CapturingSocket::new();
        let result = sd_state
            .send_offer_service(&mut [0u8; crate::UDP_BUFFER_SIZE], &config, &sock)
            .await;

        assert!(
            matches!(
                result,
                Err(Error::Protocol(crate::protocol::Error::Sd(
                    sd::Error::InvalidTtl(0)
                )))
            ),
            "got {result:?}"
        );
        assert!(sock.drain_sent().is_empty());
    }

    #[tokio::test]
    async fn send_offer_service_through_mock_propagates_socket_errors() {
        let config = ServerConfig::new(TEST_SERVICE_ID, TEST_INSTANCE_ID)
            .with_interface(Ipv4Addr::LOCALHOST)
            .with_local_port(TEST_ADVERTISED_PORT);
        let sd_state = SdStateManager::with_initial(0x1233);
        let sock = FailingSocket;
        let result = sd_state
            .send_offer_service(&mut [0u8; crate::UDP_BUFFER_SIZE], &config, &sock)
            .await;
        // Narrow assertion: the error must specifically be the
        // `Io(NetworkUnreachable)` propagated from `FailingSocket::send_to`.
        // `Err(_)` would also pass on unrelated regressions (encoding
        // failures, internal panics) and falsely attribute them to
        // socket-error propagation.
        match result {
            Err(Error::Transport(TransportError::Io(IoErrorKind::NetworkUnreachable))) => {}
            other => panic!(
                "expected Err(Transport(Io(NetworkUnreachable))) propagated from \
                 FailingSocket::send_to; got {other:?}",
            ),
        }
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

    /// Bind a sender [`TokioSocket`] on an ephemeral port with
    /// `multicast_if` pinned to the loopback interface so emitted
    /// packets loop back to any receiver joined to the same group on
    /// that interface. Uses the [`TransportFactory`] surface so the
    /// resulting socket implements [`crate::transport::TransportSocket`]
    /// — which is what the now-generic
    /// [`SdStateManager::send_offer_service`] requires.
    async fn build_mcast_sender(
        interface: Ipv4Addr,
    ) -> Result<TokioSocket, crate::transport::TransportError> {
        let mut opts = SocketOptions::new();
        opts.reuse_address = true;
        opts.reuse_port = true;
        opts.multicast_if_v4 = Some(interface);
        opts.multicast_loop_v4 = Some(true);
        crate::tokio_transport::TokioTransport
            .bind(SocketAddrV4::new(interface, 0), &opts)
            .await
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
    async fn mcast_rx_tx() -> (UdpSocket, TokioSocket) {
        let interface = Ipv4Addr::LOCALHOST;
        let rx = build_mcast_receiver(interface).expect("bind receiver");
        rx.join_multicast_v4(sd::MULTICAST_IP, interface)
            .expect("join SD multicast group");
        let tx = build_mcast_sender(interface).await.expect("bind sender");
        (rx, tx)
    }

    #[ignore = "requires MULTICAST on loopback; skipped on hosts whose `lo` \
                lacks the MULTICAST flag. Runs in any environment where \
                loopback multicast is available."]
    #[tokio::test]
    async fn send_offer_service_emits_parseable_offer_to_multicast() {
        let config = ServerConfig::new(TEST_SERVICE_ID, TEST_INSTANCE_ID)
            .with_interface(Ipv4Addr::LOCALHOST)
            .with_local_port(TEST_ADVERTISED_PORT);
        let (rx, tx) = mcast_rx_tx().await;

        // Seed with a recognisable value so on-wire session_id is exact.
        let sd_state = SdStateManager::with_initial(0x1233);
        sd_state
            .send_offer_service(&mut [0u8; crate::UDP_BUFFER_SIZE], &config, &tx)
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
        let config = ServerConfig::new(TEST_SERVICE_ID, TEST_INSTANCE_ID)
            .with_interface(Ipv4Addr::LOCALHOST)
            .with_local_port(TEST_ADVERTISED_PORT);
        let (rx, tx) = mcast_rx_tx().await;

        let sd_state = SdStateManager::with_initial(0x1233);
        sd_state
            .send_offer_service(&mut [0u8; crate::UDP_BUFFER_SIZE], &config, &tx)
            .await
            .unwrap();
        sd_state
            .send_offer_service(&mut [0u8; crate::UDP_BUFFER_SIZE], &config, &tx)
            .await
            .unwrap();

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
        let config = ServerConfig::new(TEST_SERVICE_ID, TEST_INSTANCE_ID)
            .with_interface(Ipv4Addr::LOCALHOST)
            .with_local_port(TEST_ADVERTISED_PORT);
        let (rx, tx) = mcast_rx_tx().await;

        let sd_state = SdStateManager::with_initial(0xFFFE);
        sd_state
            .send_offer_service(&mut [0u8; crate::UDP_BUFFER_SIZE], &config, &tx)
            .await
            .unwrap();
        sd_state
            .send_offer_service(&mut [0u8; crate::UDP_BUFFER_SIZE], &config, &tx)
            .await
            .unwrap();

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
}
