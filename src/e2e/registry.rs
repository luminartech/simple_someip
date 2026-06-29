//! E2E configuration registry for runtime E2E management.
//!
//! Backed by [`heapless::index_map::FnvIndexMap`] so the registry is
//! `no_std`-compatible and allocates no heap memory after construction.
//! The capacity is bounded at compile time to [`E2E_REGISTRY_CAP`]; the
//! registry rejects further registrations once that cap is reached
//! rather than silently dropping or growing — see [`E2ERegistry::register`]
//! and [`E2ERegistryFull`].

use core::net::IpAddr;

use heapless::index_map::{Entry, FnvIndexMap};

use super::{E2ECheckStatus, E2EKey, E2EProfile, E2EState, Error, e2e_check, e2e_protect};

/// Maximum number of distinct `(key → profile)` bindings the registry
/// can hold. Sized for typical workloads where a single service
/// instance has at most a few dozen E2E-protected message types.
///
/// Must be a power of two for [`FnvIndexMap`]; the `const _` assertion
/// below catches any future change that would violate the requirement.
pub const E2E_REGISTRY_CAP: usize = 32;

const _: () = assert!(
    E2E_REGISTRY_CAP.is_power_of_two(),
    "E2E_REGISTRY_CAP must be a power of two for heapless::FnvIndexMap"
);

/// Maximum number of distinct `(source, key)` **receive** counter slots
/// the registry can hold at once.
///
/// On a shared subnet the receive state is keyed per source (see
/// [`E2ERegistry`]), so this bounds *sources × keys*, not just keys —
/// size it for the high-water mark of distinct senders the node expects
/// to demux concurrently. Once full, [`E2ERegistry::check`] still runs
/// (CRC is always validated) but a brand-new source falls back to a
/// transient per-call counter, so its *sequence* continuity is not
/// tracked until a slot frees via [`E2ERegistry::reset_source`] /
/// [`E2ERegistry::unregister`]. A one-shot `warn!` fires the first time
/// this happens.
///
/// Must be a power of two for [`FnvIndexMap`].
pub const E2E_RX_STATE_CAP: usize = 64;

const _: () = assert!(
    E2E_RX_STATE_CAP.is_power_of_two(),
    "E2E_RX_STATE_CAP must be a power of two for heapless::FnvIndexMap"
);

/// Returned by [`E2ERegistry::register`] when the registry is at
/// capacity.
///
/// The contained value is the cap that was hit (i.e.
/// [`E2E_REGISTRY_CAP`]); kept in the error so log lines and panic
/// messages name the constant the user can adjust.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("e2e registry at capacity ({0})")]
pub struct E2ERegistryFull(pub usize);

/// Registry mapping message keys to E2E profile configurations and the
/// per-source / per-key counter state.
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
///
/// `no_std`-friendly: every map is a fixed-capacity [`FnvIndexMap`], so
/// construction and the entire lifetime of the registry are heap-free.
/// Construction is `const`, so a `static` instance can be declared in
/// firmware boot code. Profile/transmit slots are bounded by
/// [`E2E_REGISTRY_CAP`]; receive slots by [`E2E_RX_STATE_CAP`].
#[derive(Debug)]
pub struct E2ERegistry {
    /// Endpoint-agnostic profile configuration, keyed by data element.
    configs: FnvIndexMap<E2EKey, E2EProfile, E2E_REGISTRY_CAP>,
    /// Receive counter state, per `(source, key)`.
    rx_states: FnvIndexMap<(IpAddr, E2EKey), E2EState, E2E_RX_STATE_CAP>,
    /// Transmit counter state, per key.
    tx_states: FnvIndexMap<E2EKey, E2EState, E2E_REGISTRY_CAP>,
    /// Latches the one-shot `warn!` emitted when `rx_states` first
    /// saturates, so an over-capacity subnet doesn't flood the logs.
    rx_saturation_warned: bool,
}

impl E2ERegistry {
    /// Create an empty registry. `const`-constructible so it can live
    /// in `static` storage on bare-metal targets.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            configs: FnvIndexMap::new(),
            rx_states: FnvIndexMap::new(),
            tx_states: FnvIndexMap::new(),
            rx_saturation_warned: false,
        }
    }

    /// Register an E2E profile for the given key, creating fresh transmit
    /// state and clearing any prior per-source receive state for the key.
    ///
    /// Replacing the profile of an already-registered key always
    /// succeeds (the existing slots are reused). Adding a new key when
    /// the registry already holds [`E2E_REGISTRY_CAP`] entries returns
    /// [`Err(E2ERegistryFull)`](E2ERegistryFull); the caller is
    /// responsible for sizing the cap to its workload's high-water
    /// mark.
    ///
    /// # Errors
    ///
    /// [`E2ERegistryFull`] when the registry is full and `key` is not
    /// already present.
    pub fn register(&mut self, key: E2EKey, profile: E2EProfile) -> Result<(), E2ERegistryFull> {
        let state = E2EState::from_profile(&profile);
        // `FnvIndexMap::insert` returns `Err((K, V))` only when the map is
        // full AND `key` is not already present (replacing an existing
        // entry never overflows). `configs` and `tx_states` share both the
        // key set and `E2E_REGISTRY_CAP`, so we gate on `configs` first and
        // the `tx_states` insert below can only ever replace-in-place.
        if self.configs.insert(key, profile).is_err() {
            return Err(E2ERegistryFull(E2E_REGISTRY_CAP));
        }
        let _ = self.tx_states.insert(key, state);
        // A re-register restarts the counter, so drop stale per-source
        // receive state for this key.
        self.rx_states.retain(|(_, k), _| *k != key);
        Ok(())
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
        // Per-source receive state, created lazily the first time a
        // `(source, key)` pair is seen. When `rx_states` is at
        // [`E2E_RX_STATE_CAP`] a brand-new source can't claim a slot; fall
        // back to a transient counter so the CRC is still validated (only
        // sequence continuity is lost) and warn once.
        match self.rx_states.entry((source, key)) {
            Entry::Occupied(occupied) => {
                let state = occupied.into_mut();
                Some(e2e_check(profile, state, payload, upper_header))
            }
            Entry::Vacant(vacant) => match vacant.insert(E2EState::from_profile(profile)) {
                Ok(state) => Some(e2e_check(profile, state, payload, upper_header)),
                Err(_full) => {
                    if !self.rx_saturation_warned {
                        self.rx_saturation_warned = true;
                        crate::log::warn!(
                            "E2E rx_states at capacity ({}); source {} falls back to a \
                             transient counter — sequence continuity untracked until a slot frees",
                            E2E_RX_STATE_CAP,
                            source
                        );
                    }
                    let mut transient = E2EState::from_profile(profile);
                    Some(e2e_check(profile, &mut transient, payload, upper_header))
                }
            },
        }
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
    use core::net::Ipv4Addr;

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
        reg.register(key, E2EProfile::Profile4(config.clone()))
            .expect("register fits within E2E_REGISTRY_CAP");
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
        reg.register(key, make_profile5())
            .expect("register fits within E2E_REGISTRY_CAP");

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
        sender
            .register(key, make_profile5())
            .expect("register fits within E2E_REGISTRY_CAP");
        let mut b0 = [0u8; 64];
        let l0 = protect_next(&mut sender, key, &mut b0);
        let mut b1 = [0u8; 64];
        let l1 = protect_next(&mut sender, key, &mut b1);

        let mut recv = E2ERegistry::new();
        recv.register(key, make_profile5())
            .expect("register fits within E2E_REGISTRY_CAP");

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
        sender
            .register(key, make_profile5())
            .expect("register fits within E2E_REGISTRY_CAP");
        let mut b0 = [0u8; 64];
        let l0 = protect_next(&mut sender, key, &mut b0);
        let mut b1 = [0u8; 64];
        let l1 = protect_next(&mut sender, key, &mut b1);

        let mut recv = E2ERegistry::new();
        recv.register(key, make_profile5())
            .expect("register fits within E2E_REGISTRY_CAP");
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
        reg.register(key, E2EProfile::Profile4(Profile4Config::new(0, 15)))
            .expect("register fits within E2E_REGISTRY_CAP");
        assert!(reg.contains_key(&key));
        reg.unregister(&key);
        assert!(!reg.contains_key(&key));
    }

    #[test]
    fn default_is_empty() {
        let reg = E2ERegistry::default();
        assert!(!reg.contains_key(&make_key()));
    }

    /// Replacing the profile of an already-registered key MUST succeed
    /// even when the registry is at capacity — the slot is reused, not
    /// added. Regression guard for the FnvIndexMap "full + missing key"
    /// branch.
    #[test]
    fn register_replacement_succeeds_when_full() {
        let mut reg = E2ERegistry::new();
        for i in 0..E2E_REGISTRY_CAP {
            let key = E2EKey::new(0x1000 + u16::try_from(i).unwrap(), 0);
            reg.register(key, E2EProfile::Profile4(Profile4Config::new(0, 15)))
                .expect("filling to cap");
        }
        // Re-register the first key with a different profile — must succeed.
        let key0 = E2EKey::new(0x1000, 0);
        let result = reg.register(key0, E2EProfile::Profile4(Profile4Config::new(42, 15)));
        assert!(
            result.is_ok(),
            "replacing an existing entry must succeed even at capacity"
        );
    }

    /// Adding a new key beyond the cap MUST return
    /// `Err(E2ERegistryFull(E2E_REGISTRY_CAP))` and leave the registry
    /// otherwise unchanged. Regression test that locks in the
    /// capacity contract documented on `register`.
    #[test]
    fn register_overflow_returns_err_and_does_not_mutate() {
        let mut reg = E2ERegistry::new();
        for i in 0..E2E_REGISTRY_CAP {
            reg.register(
                E2EKey::new(0x2000 + u16::try_from(i).unwrap(), 0),
                E2EProfile::Profile4(Profile4Config::new(0, 15)),
            )
            .expect("filling to cap");
        }
        // The (cap+1)-th distinct key must be rejected.
        let overflow_key = E2EKey::new(0xFFFE, 0);
        let err = reg
            .register(
                overflow_key,
                E2EProfile::Profile4(Profile4Config::new(0, 15)),
            )
            .expect_err("registering the (cap+1)-th key must overflow");
        assert_eq!(err, E2ERegistryFull(E2E_REGISTRY_CAP));
        // And the rejected key must NOT be present.
        assert!(!reg.contains_key(&overflow_key));
    }
}
