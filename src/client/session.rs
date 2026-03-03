use std::{collections::HashMap, net::SocketAddr};

/// Distinguishes multicast vs unicast transport for per-sender session tracking.
/// The AUTOSAR spec requires separate session ID tracking per transport.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TransportKind {
    Multicast,
    #[allow(dead_code)]
    Unicast,
}

/// Composite key identifying a sender on a specific transport.
type SessionKey = (SocketAddr, TransportKind);

/// Per-sender session state for reboot detection.
#[derive(Clone, Copy, Debug)]
struct SenderSessionState {
    last_session_id: u16,
    last_reboot_flag: bool,
}

/// Result of checking a sender's session ID and reboot flag against stored state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionVerdict {
    /// Session is valid (normal increment or first message with matching state).
    Ok,
    /// Sender has rebooted (reboot flag transition or session ID decrease).
    Reboot,
    /// First message ever seen from this sender on this transport.
    Initial,
}

/// Tracks per-sender session state for reboot detection.
///
/// Per the AUTOSAR SOME/IP-SD spec, a reboot is detected when:
/// - The reboot flag transitions from 0 to 1
/// - The session ID decreases while the reboot flag remains 1
#[derive(Debug, Default)]
pub struct SessionTracker {
    state: HashMap<SessionKey, SenderSessionState>,
}

impl SessionTracker {
    /// Check the session ID and reboot flag from a sender and return a verdict.
    /// Always updates the stored state after the check.
    pub fn check(
        &mut self,
        sender: SocketAddr,
        transport: TransportKind,
        session_id: u16,
        reboot_flag: bool,
    ) -> SessionVerdict {
        let key = (sender, transport);
        let verdict = match self.state.get(&key) {
            None => SessionVerdict::Initial,
            Some(prev) => {
                if !prev.last_reboot_flag && reboot_flag {
                    // Reboot flag 0 -> 1 transition
                    SessionVerdict::Reboot
                } else if prev.last_reboot_flag && reboot_flag && session_id < prev.last_session_id
                {
                    // Session ID decreased while reboot flag stays 1
                    SessionVerdict::Reboot
                } else {
                    SessionVerdict::Ok
                }
            }
        };
        self.state.insert(
            key,
            SenderSessionState {
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

    #[test]
    fn first_message_returns_initial() {
        let mut tracker = SessionTracker::default();
        let verdict = tracker.check(addr(1000), TransportKind::Multicast, 1, true);
        assert_eq!(verdict, SessionVerdict::Initial);
    }

    #[test]
    fn normal_increment_returns_ok() {
        let mut tracker = SessionTracker::default();
        tracker.check(addr(1000), TransportKind::Multicast, 1, true);
        let verdict = tracker.check(addr(1000), TransportKind::Multicast, 2, true);
        assert_eq!(verdict, SessionVerdict::Ok);
    }

    #[test]
    fn reboot_flag_0_to_1_returns_reboot() {
        let mut tracker = SessionTracker::default();
        tracker.check(addr(1000), TransportKind::Multicast, 100, false);
        let verdict = tracker.check(addr(1000), TransportKind::Multicast, 1, true);
        assert_eq!(verdict, SessionVerdict::Reboot);
    }

    #[test]
    fn session_id_decrease_with_reboot_flag_1_returns_reboot() {
        let mut tracker = SessionTracker::default();
        tracker.check(addr(1000), TransportKind::Multicast, 100, true);
        let verdict = tracker.check(addr(1000), TransportKind::Multicast, 50, true);
        assert_eq!(verdict, SessionVerdict::Reboot);
    }

    #[test]
    fn session_id_decrease_with_reboot_flag_0_returns_ok() {
        let mut tracker = SessionTracker::default();
        tracker.check(addr(1000), TransportKind::Multicast, 100, false);
        // Session ID decrease while reboot flag is 0 is not a reboot signal
        let verdict = tracker.check(addr(1000), TransportKind::Multicast, 50, false);
        assert_eq!(verdict, SessionVerdict::Ok);
    }

    #[test]
    fn different_transports_tracked_separately() {
        let mut tracker = SessionTracker::default();
        tracker.check(addr(1000), TransportKind::Multicast, 100, true);
        // Same sender, different transport — first message on Unicast
        let verdict = tracker.check(addr(1000), TransportKind::Unicast, 1, true);
        assert_eq!(verdict, SessionVerdict::Initial);
    }

    #[test]
    fn different_senders_tracked_separately() {
        let mut tracker = SessionTracker::default();
        tracker.check(addr(1000), TransportKind::Multicast, 100, true);
        // Different sender — first message
        let verdict = tracker.check(addr(2000), TransportKind::Multicast, 1, true);
        assert_eq!(verdict, SessionVerdict::Initial);
    }

    #[test]
    fn reboot_flag_1_to_0_returns_ok() {
        let mut tracker = SessionTracker::default();
        tracker.check(addr(1000), TransportKind::Multicast, 100, true);
        // Reboot flag going 1->0 is not a reboot (it means session ID wrapped)
        let verdict = tracker.check(addr(1000), TransportKind::Multicast, 101, false);
        assert_eq!(verdict, SessionVerdict::Ok);
    }

    #[test]
    fn same_session_id_with_reboot_flag_1_returns_ok() {
        let mut tracker = SessionTracker::default();
        tracker.check(addr(1000), TransportKind::Multicast, 5, true);
        // Same session ID (not a decrease) should be OK
        let verdict = tracker.check(addr(1000), TransportKind::Multicast, 5, true);
        assert_eq!(verdict, SessionVerdict::Ok);
    }
}
