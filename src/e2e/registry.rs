//! E2E configuration registry for runtime E2E management.

use std::collections::HashMap;
use std::net::IpAddr;

use super::{E2ECheckStatus, E2EKey, E2EProfile, E2EState, Error, e2e_check, e2e_protect};

/// Registry mapping message keys to E2E profile configurations and counter
/// state.
///
/// On a shared subnet several devices send the same `(service, method)` under
/// the same fixed instance id. The profile *configuration* is endpoint-agnostic
/// (one per [`E2EKey`]), but the **receive** counter state must be independent
/// per device — otherwise two senders' interleaved counters collide into
/// spurious `WrongSequence` results. Receive state is therefore keyed by
/// `(source, key)` and created lazily the first time a source is seen.
///
/// Transmit (protect) counter state stays per-key: a fan-out publish sends the
/// same protected bytes (one counter) to every subscriber, and per-recipient
/// transmit counters are handled a layer up (e.g. `iris_someip_client`).
#[derive(Debug)]
pub struct E2ERegistry {
    /// Endpoint-agnostic profile configuration, keyed by data element.
    configs: HashMap<E2EKey, E2EProfile>,
    /// Receive counter state, per source address.
    rx_states: HashMap<(IpAddr, E2EKey), E2EState>,
    /// Transmit counter state, per key.
    tx_states: HashMap<E2EKey, E2EState>,
}

impl E2ERegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            configs: HashMap::new(),
            rx_states: HashMap::new(),
            tx_states: HashMap::new(),
        }
    }

    /// Register an E2E profile for the given key, creating fresh transmit state
    /// and clearing any prior per-source receive state for the key.
    pub fn register(&mut self, key: E2EKey, profile: E2EProfile) {
        self.tx_states.insert(key, E2EState::from_profile(&profile));
        self.rx_states.retain(|(_, k), _| *k != key);
        self.configs.insert(key, profile);
    }

    /// Remove E2E configuration (and all state) for the given key.
    pub fn unregister(&mut self, key: &E2EKey) {
        self.configs.remove(key);
        self.tx_states.remove(key);
        self.rx_states.retain(|(_, k), _| k != key);
    }

    /// Returns `true` if a profile is registered for `key`.
    #[must_use]
    pub fn contains_key(&self, key: &E2EKey) -> bool {
        self.configs.contains_key(key)
    }

    /// Run E2E check for `key` against `source`'s receive counter state, if
    /// configured.
    ///
    /// Returns `None` if no profile is registered for `key`. Otherwise returns
    /// the check status and the best available payload (stripped E2E header on
    /// success, original bytes on check failure).
    pub fn check<'a>(
        &mut self,
        source: IpAddr,
        key: E2EKey,
        payload: &'a [u8],
        upper_header: [u8; 8],
    ) -> Option<(E2ECheckStatus, &'a [u8])> {
        let profile = self.configs.get(&key)?;
        let state = self
            .rx_states
            .entry((source, key))
            .or_insert_with(|| E2EState::from_profile(profile));
        Some(e2e_check(profile, state, payload, upper_header))
    }

    /// Run E2E protect for `key` if configured.
    ///
    /// Returns `None` if no profile is registered for `key`.
    pub fn protect(
        &mut self,
        key: E2EKey,
        payload: &[u8],
        upper_header: [u8; 8],
        output: &mut [u8],
    ) -> Option<Result<usize, Error>> {
        let profile = self.configs.get(&key)?;
        let state = self.tx_states.get_mut(&key)?;
        Some(e2e_protect(profile, state, payload, upper_header, output))
    }

    /// Drop all per-source receive state for `source` (e.g. on its reboot), so
    /// its next frame starts a fresh counter sequence. Configuration and
    /// transmit state are untouched.
    pub fn reset_source(&mut self, source: IpAddr) {
        self.rx_states.retain(|(s, _), _| *s != source);
    }
}

impl Default for E2ERegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::e2e::{Profile4Config, Profile5Config};
    use std::net::Ipv4Addr;

    fn make_key() -> E2EKey {
        E2EKey::new(0x1234, 0x5678)
    }

    fn src() -> IpAddr {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    }

    fn make_profile5() -> E2EProfile {
        E2EProfile::Profile5(Profile5Config::new(0x1234, 20, 15))
    }

    /// Protect a 20-byte "Hello" frame with `sender`'s next transmit counter,
    /// writing into `out` and returning the protected length. Avoids `Vec`
    /// because the crate's prelude is `core` (no_std-compatible).
    fn protect_next(sender: &mut E2ERegistry, key: E2EKey, out: &mut [u8; 64]) -> usize {
        let mut payload = [0u8; 20];
        payload[..5].copy_from_slice(b"Hello");
        sender.protect(key, &payload, [0; 8], out).unwrap().unwrap()
    }

    #[test]
    fn register_and_check_profile4() {
        let mut reg = E2ERegistry::new();
        let key = make_key();
        let config = Profile4Config::new(0x12345678, 15);
        reg.register(key, E2EProfile::Profile4(config.clone()));
        assert!(reg.contains_key(&key));

        // Protect a payload
        let payload = b"Hello";
        let mut out = [0u8; 64];
        let len = reg
            .protect(key, payload, [0; 8], &mut out)
            .unwrap()
            .unwrap();

        // Check it
        let (status, stripped) = reg.check(src(), key, &out[..len], [0; 8]).unwrap();
        assert_eq!(status, E2ECheckStatus::Ok);
        assert_eq!(stripped, payload);
    }

    #[test]
    fn register_and_check_profile5() {
        let mut reg = E2ERegistry::new();
        let key = make_key();
        reg.register(key, make_profile5());

        let mut payload = [0u8; 20];
        payload[..5].copy_from_slice(b"Hello");
        let mut out = [0u8; 64];
        let len = reg
            .protect(key, &payload, [0; 8], &mut out)
            .unwrap()
            .unwrap();

        let (status, stripped) = reg.check(src(), key, &out[..len], [0; 8]).unwrap();
        assert_eq!(status, E2ECheckStatus::Ok);
        assert_eq!(stripped, &payload);
    }

    #[test]
    fn distinct_sources_have_independent_e2e_state() {
        let a = IpAddr::V4(Ipv4Addr::new(192, 168, 11, 101));
        let b = IpAddr::V4(Ipv4Addr::new(192, 168, 11, 102));
        let key = make_key();

        // A sender produces two frames carrying counters 0 then 1.
        let mut sender = E2ERegistry::new();
        sender.register(key, make_profile5());
        let mut b0 = [0u8; 64];
        let l0 = protect_next(&mut sender, key, &mut b0);
        let mut b1 = [0u8; 64];
        let l1 = protect_next(&mut sender, key, &mut b1);

        let mut recv = E2ERegistry::new();
        recv.register(key, make_profile5());

        // Source A consumes counters 0 then 1.
        assert_eq!(
            recv.check(a, key, &b0[..l0], [0; 8]).unwrap().0,
            E2ECheckStatus::Ok
        );
        assert_eq!(
            recv.check(a, key, &b1[..l1], [0; 8]).unwrap().0,
            E2ECheckStatus::Ok
        );
        // Source B, interleaved AFTER A, starts its own counter sequence at 0.
        // With shared (per-key) receive state this would flag b0 as
        // out-of-sequence because A already advanced the single counter past 0.
        assert_eq!(
            recv.check(b, key, &b0[..l0], [0; 8]).unwrap().0,
            E2ECheckStatus::Ok,
            "source B's receive counter must be independent of source A's"
        );
        assert_eq!(
            recv.check(b, key, &b1[..l1], [0; 8]).unwrap().0,
            E2ECheckStatus::Ok
        );
    }

    #[test]
    fn reset_source_clears_only_that_source() {
        let a = IpAddr::V4(Ipv4Addr::new(192, 168, 11, 101));
        let key = make_key();

        let mut sender = E2ERegistry::new();
        sender.register(key, make_profile5());
        let mut b0 = [0u8; 64];
        let l0 = protect_next(&mut sender, key, &mut b0);
        let mut b1 = [0u8; 64];
        let l1 = protect_next(&mut sender, key, &mut b1);

        let mut recv = E2ERegistry::new();
        recv.register(key, make_profile5());
        recv.check(a, key, &b0[..l0], [0; 8]);
        recv.check(a, key, &b1[..l1], [0; 8]);

        // After a reboot, source A starts fresh — its counter-0 frame is Ok
        // again.
        recv.reset_source(a);
        assert_eq!(
            recv.check(a, key, &b0[..l0], [0; 8]).unwrap().0,
            E2ECheckStatus::Ok,
            "reset_source(a) restarts A's receive counter sequence"
        );
    }

    #[test]
    fn unregistered_key_returns_none() {
        let mut reg = E2ERegistry::new();
        let key = make_key();
        assert!(!reg.contains_key(&key));
        assert!(reg.check(src(), key, b"test", [0; 8]).is_none());
        assert!(reg.protect(key, b"test", [0; 8], &mut [0; 64]).is_none());
    }

    #[test]
    fn unregister_removes_key() {
        let mut reg = E2ERegistry::new();
        let key = make_key();
        reg.register(key, E2EProfile::Profile4(Profile4Config::new(0, 15)));
        assert!(reg.contains_key(&key));
        reg.unregister(&key);
        assert!(!reg.contains_key(&key));
    }

    #[test]
    fn default_is_empty() {
        let reg = E2ERegistry::default();
        assert!(!reg.contains_key(&make_key()));
    }
}
