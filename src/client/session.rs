use crate::protocol::sd::RebootFlag;
use std::{collections::HashMap, net::SocketAddr};

/// Distinguishes multicast vs unicast transport for per-sender session tracking.
/// The AUTOSAR spec requires separate session ID tracking per transport.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TransportKind {
    Multicast,
    #[allow(dead_code)]
    Unicast,
}

/// Composite key identifying a specific service instance from a sender on a
/// specific transport. Tracking per service instance avoids false reboot
/// detection when a sender interleaves SD offers for multiple services, each
/// with its own independent session counter.
type SessionKey = (SocketAddr, TransportKind, u16, u16);

/// Per-service-instance session state for reboot detection.
#[derive(Clone, Copy, Debug)]
struct SessionState {
    last_session_id: u16,
    last_reboot_flag: RebootFlag,
}

/// Result of checking a sender's session ID and reboot flag against stored state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionVerdict {
    /// Session is valid (normal increment or first message with matching state).
    Ok,
    /// Sender has rebooted (reboot flag 0→1 transition, or session ID decreased
    /// while reboot flag remains 1 within the same service instance stream).
    Reboot,
    /// First message ever seen from this service instance on this transport.
    Initial,
}

/// Tracks per-service-instance session state for reboot detection.
///
/// A reboot is detected when, for a given `(sender, transport, service_id,
/// instance_id)` tuple:
/// - The reboot flag transitions from 0 to 1, **or**
/// - The session ID decreases while the reboot flag remains 1
///
/// Tracking per service instance (rather than per sender) avoids false
/// positives when a sensor interleaves SD offers for multiple services
/// with independent session counters on the same source address.
#[derive(Debug, Default)]
pub struct SessionTracker {
    state: HashMap<SessionKey, SessionState>,
}

impl SessionTracker {
    /// Check the session ID and reboot flag for a specific service instance
    /// and return a verdict. Always updates the stored state after the check.
    ///
    /// Call this once per service entry in an SD message (not once per message),
    /// so each service instance gets its own session counter.
    pub fn check(
        &mut self,
        sender: SocketAddr,
        transport: TransportKind,
        service_id: u16,
        instance_id: u16,
        session_id: u16,
        reboot_flag: RebootFlag,
    ) -> SessionVerdict {
        let key = (sender, transport, service_id, instance_id);
        let verdict = match self.state.get(&key) {
            None => SessionVerdict::Initial,
            Some(prev) => {
                if prev.last_reboot_flag == RebootFlag::Continuous
                    && reboot_flag == RebootFlag::RecentlyRebooted
                {
                    // Continuous → RecentlyRebooted transition — authoritative reboot signal
                    SessionVerdict::Reboot
                } else if prev.last_reboot_flag == RebootFlag::RecentlyRebooted
                    && reboot_flag == RebootFlag::RecentlyRebooted
                    && session_id < prev.last_session_id
                    && !(prev.last_session_id == u16::MAX && session_id <= 1)
                {
                    // Session ID decreased within the same service instance
                    // while reboot flag stays `RecentlyRebooted` — this is a reboot.
                    // Exception: 0xFFFF→1 is the spec-compliant counter wrap; 0xFFFF→0
                    // is tolerated for non-compliant implementations. Neither is a reboot.
                    SessionVerdict::Reboot
                } else {
                    SessionVerdict::Ok
                }
            }
        };
        self.state.insert(
            key,
            SessionState {
                last_session_id: session_id,
                last_reboot_flag: reboot_flag,
            },
        );
        verdict
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddr};

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::new(Ipv4Addr::new(192, 168, 1, 10).into(), port)
    }

    const SVC: u16 = 0x0047;
    const INST: u16 = 0x0001;
    const SVC_B: u16 = 0x005D;
    const RB: RebootFlag = RebootFlag::RecentlyRebooted;
    const CONT: RebootFlag = RebootFlag::Continuous;

    #[test]
    fn first_message_returns_initial() {
        let mut tracker = SessionTracker::default();
        let verdict = tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 1, RB);
        assert_eq!(verdict, SessionVerdict::Initial);
    }

    #[test]
    fn normal_increment_returns_ok() {
        let mut tracker = SessionTracker::default();
        tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 1, RB);
        let verdict = tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 2, RB);
        assert_eq!(verdict, SessionVerdict::Ok);
    }

    #[test]
    fn reboot_flag_continuous_to_recently_rebooted_returns_reboot() {
        let mut tracker = SessionTracker::default();
        tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 100, CONT);
        let verdict = tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 1, RB);
        assert_eq!(verdict, SessionVerdict::Reboot);
    }

    #[test]
    fn session_id_decrease_same_service_with_recently_rebooted_returns_reboot() {
        // Within a single service instance, session ID decrease is a real reboot.
        let mut tracker = SessionTracker::default();
        tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 100, RB);
        let verdict = tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 50, RB);
        assert_eq!(verdict, SessionVerdict::Reboot);
    }

    #[test]
    fn session_id_decrease_different_services_no_false_reboot() {
        // Different service instances have independent counters — interleaving
        // does not cause false reboots.
        let mut tracker = SessionTracker::default();
        tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 100, RB);
        // Different service, lower session ID — this is Initial, not Reboot.
        let verdict = tracker.check(addr(1000), TransportKind::Multicast, SVC_B, INST, 50, RB);
        assert_eq!(verdict, SessionVerdict::Initial);
    }

    #[test]
    fn interleaved_sd_offers_no_false_reboot() {
        // Simulates the real-world scenario: sensor sends alternating SD offers
        // for service A (session 1,2,3...) and service B (session 1,2,3...).
        // The old per-sender tracking would see: 1, 1(decrease!), 2, 2(decrease!), ...
        // Per-service tracking sees each stream independently.
        let mut tracker = SessionTracker::default();
        // Service A: session 1
        tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 1, RB);
        // Service B: session 1 (would have been "decrease" with per-sender tracking)
        let v = tracker.check(addr(1000), TransportKind::Multicast, SVC_B, INST, 1, RB);
        assert_eq!(v, SessionVerdict::Initial);
        // Service A: session 2
        let v = tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 2, RB);
        assert_eq!(v, SessionVerdict::Ok);
        // Service B: session 2
        let v = tracker.check(addr(1000), TransportKind::Multicast, SVC_B, INST, 2, RB);
        assert_eq!(v, SessionVerdict::Ok);
    }

    #[test]
    fn session_id_decrease_with_continuous_returns_ok() {
        let mut tracker = SessionTracker::default();
        tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 100, CONT);
        // Session ID decrease while Continuous — not a reboot
        let verdict = tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 50, CONT);
        assert_eq!(verdict, SessionVerdict::Ok);
    }

    #[test]
    fn different_transports_tracked_separately() {
        let mut tracker = SessionTracker::default();
        tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 100, RB);
        // Same sender+service, different transport — first message on Unicast
        let verdict = tracker.check(addr(1000), TransportKind::Unicast, SVC, INST, 1, RB);
        assert_eq!(verdict, SessionVerdict::Initial);
    }

    #[test]
    fn different_senders_tracked_separately() {
        let mut tracker = SessionTracker::default();
        tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 100, RB);
        // Different sender — first message
        let verdict = tracker.check(addr(2000), TransportKind::Multicast, SVC, INST, 1, RB);
        assert_eq!(verdict, SessionVerdict::Initial);
    }

    #[test]
    fn reboot_flag_recently_rebooted_to_continuous_returns_ok() {
        let mut tracker = SessionTracker::default();
        tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 100, RB);
        // RecentlyRebooted→Continuous is not a reboot (it means session ID wrapped)
        let verdict = tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 101, CONT);
        assert_eq!(verdict, SessionVerdict::Ok);
    }

    #[test]
    fn same_session_id_with_recently_rebooted_returns_ok() {
        let mut tracker = SessionTracker::default();
        tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 5, RB);
        // Same session ID (not a decrease) should be OK
        let verdict = tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 5, RB);
        assert_eq!(verdict, SessionVerdict::Ok);
    }

    #[test]
    fn different_instance_ids_tracked_separately() {
        let mut tracker = SessionTracker::default();
        tracker.check(addr(1000), TransportKind::Multicast, SVC, 0x0001, 100, RB);
        // Same service, different instance — first message
        let verdict = tracker.check(addr(1000), TransportKind::Multicast, SVC, 0x0002, 1, RB);
        assert_eq!(verdict, SessionVerdict::Initial);
    }

    #[test]
    fn session_id_wrap_around_returns_ok() {
        // 0xFFFF→1 with RecentlyRebooted is a normal counter wrap, not a reboot.
        let mut tracker = SessionTracker::default();
        tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 65535, RB);
        let verdict = tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 1, RB);
        assert_eq!(verdict, SessionVerdict::Ok);
    }

    #[test]
    fn session_id_wrap_around_then_normal_increment() {
        // After a wrap (0xFFFF→1), normal incrementing should continue as Ok.
        let mut tracker = SessionTracker::default();
        tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 65535, RB);
        tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 1, RB);
        let verdict = tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 2, RB);
        assert_eq!(verdict, SessionVerdict::Ok);
    }

    #[test]
    fn session_id_wrap_to_zero_returns_ok() {
        // 0xFFFF→0: non-spec-compliant wrap scheme, still treated as a normal wrap.
        let mut tracker = SessionTracker::default();
        tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 65535, RB);
        let verdict = tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 0, RB);
        assert_eq!(verdict, SessionVerdict::Ok);
    }

    #[test]
    fn reboot_flag_transition_with_session_id_decrease_both_signal_reboot() {
        // Both indicators fire at once (Continuous→RecentlyRebooted AND session ID reset).
        let mut tracker = SessionTracker::default();
        tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 100, CONT);
        let verdict = tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 1, RB);
        assert_eq!(verdict, SessionVerdict::Reboot);
    }

    #[test]
    fn multiple_reboots_in_sequence() {
        let mut tracker = SessionTracker::default();
        tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 1, RB);
        tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 50, RB);
        // First reboot (session ID decrease)
        let v = tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 1, RB);
        assert_eq!(v, SessionVerdict::Reboot);
        // Normal traffic after reboot
        let v = tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 2, RB);
        assert_eq!(v, SessionVerdict::Ok);
        // Second reboot (flag transition)
        tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 10, CONT);
        let v = tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 1, RB);
        assert_eq!(v, SessionVerdict::Reboot);
    }

    #[test]
    fn interleaved_offers_with_real_reboot() {
        // Two services interleaving, then one experiences a real reboot.
        let mut tracker = SessionTracker::default();
        tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 10, RB);
        tracker.check(addr(1000), TransportKind::Multicast, SVC_B, INST, 10, RB);
        tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 11, RB);
        tracker.check(addr(1000), TransportKind::Multicast, SVC_B, INST, 11, RB);

        // Sensor reboots — both services restart at session 1 with RecentlyRebooted.
        // Flag was already RecentlyRebooted, so session ID decrease triggers reboot.
        let v = tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 1, RB);
        assert_eq!(v, SessionVerdict::Reboot);
        let v = tracker.check(addr(1000), TransportKind::Multicast, SVC_B, INST, 1, RB);
        assert_eq!(v, SessionVerdict::Reboot);
    }

    #[test]
    fn normal_increment_with_continuous_returns_ok() {
        let mut tracker = SessionTracker::default();
        tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 1, CONT);
        let verdict = tracker.check(addr(1000), TransportKind::Multicast, SVC, INST, 2, CONT);
        assert_eq!(verdict, SessionVerdict::Ok);
    }
}
