//! E2E configuration registry for runtime E2E management.
//!
//! Backed by [`heapless::index_map::FnvIndexMap`] so the registry is
//! `no_std`-compatible and allocates no heap memory after construction.
//! The capacity is bounded at compile time by [`E2ERegistry`]'s `CAP`
//! const-generic parameter (defaulting to [`E2E_REGISTRY_CAP`]);
//! the registry rejects further registrations once that cap is reached
//! rather than silently dropping or growing — see [`E2ERegistry::register`]
//! and [`E2ERegistryFull`].

use heapless::index_map::FnvIndexMap;

use super::{E2ECheckStatus, E2EKey, E2EProfile, E2EState, Error, e2e_check, e2e_protect};

/// Default max number of distinct `(key → profile)` bindings used when
/// [`E2ERegistry`] is instantiated without an explicit const-generic
/// `CAP`. Must be a power of two (heapless invariant).
///
/// Consumers wanting a tighter footprint instantiate `E2ERegistry<CAP>`
/// directly with a value sized to their E2E-protected message catalog —
/// see [`crate::transport::StaticE2EStorage`] /
/// [`crate::transport::StaticE2EHandle`].
pub const E2E_REGISTRY_CAP: usize = 32;

const _: () = assert!(
    E2E_REGISTRY_CAP.is_power_of_two(),
    "E2E_REGISTRY_CAP must be a power of two for heapless::FnvIndexMap"
);

/// Returned by [`E2ERegistry::register`] when the registry is at
/// capacity.
///
/// The contained value is the cap that was hit; kept in the error so
/// log lines name the actual capacity the user can adjust.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("e2e registry at capacity ({0})")]
pub struct E2ERegistryFull(pub usize);

/// Registry mapping message keys to E2E profile configurations and
/// the per-key counter / sequence state.
///
/// `no_std`-friendly: backed by a fixed-capacity
/// [`FnvIndexMap`] so construction and the entire lifetime of the
/// registry are heap-free. Construction is `const`, so a `static`
/// instance can be declared in firmware boot code. The const-generic
/// `CAP` parameter (defaulting to [`DEFAULT_E2E_REGISTRY_CAP`]) lets
/// each consumer size the registry to their own service catalog.
/// `CAP` must be a power of two — `heapless::FnvIndexMap`'s build-time
/// check catches violations at monomorphization.
#[derive(Debug)]
pub struct E2ERegistry<const CAP: usize = E2E_REGISTRY_CAP> {
    map: FnvIndexMap<E2EKey, (E2EProfile, E2EState), CAP>,
}

impl<const CAP: usize> E2ERegistry<CAP> {
    /// Create an empty registry. `const`-constructible so it can live
    /// in `static` storage on bare-metal targets.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            map: FnvIndexMap::new(),
        }
    }

    /// Register an E2E profile for the given key, creating fresh state.
    ///
    /// Replacing the profile of an already-registered key always
    /// succeeds (the existing slot is reused). Adding a new key when
    /// the registry already holds `CAP` entries returns
    /// [`Err(E2ERegistryFull)`](E2ERegistryFull); the caller is
    /// responsible for sizing `CAP` to its workload's high-water
    /// mark.
    ///
    /// # Errors
    ///
    /// [`E2ERegistryFull`] when the registry is full and `key` is not
    /// already present.
    pub fn register(&mut self, key: E2EKey, profile: E2EProfile) -> Result<(), E2ERegistryFull> {
        let state = E2EState::from_profile(&profile);
        // `FnvIndexMap::insert` returns `Err((K, V))` only when the
        // map is full AND `key` is not already present (replacing an
        // existing entry never overflows).
        match self.map.insert(key, (profile, state)) {
            Ok(_) => Ok(()),
            Err(_) => Err(E2ERegistryFull(CAP)),
        }
    }

    /// Remove E2E configuration for the given key. No-op if absent.
    pub fn unregister(&mut self, key: &E2EKey) {
        self.map.remove(key);
    }

    /// Returns `true` if a profile is registered for `key`.
    #[must_use]
    pub fn contains_key(&self, key: &E2EKey) -> bool {
        self.map.contains_key(key)
    }

    /// Run E2E check for `key` if configured.
    ///
    /// Returns `None` if no profile is registered for `key`.
    /// Otherwise returns the check status and the best available payload
    /// (stripped E2E header on success, original bytes on check failure).
    pub fn check<'a>(
        &mut self,
        key: E2EKey,
        payload: &'a [u8],
        upper_header: [u8; 8],
    ) -> Option<(E2ECheckStatus, &'a [u8])> {
        let (profile, state) = self.map.get_mut(&key)?;
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
        let (profile, state) = self.map.get_mut(&key)?;
        Some(e2e_protect(profile, state, payload, upper_header, output))
    }
}

impl<const CAP: usize> Default for E2ERegistry<CAP> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::e2e::{Profile4Config, Profile5Config};

    fn make_key() -> E2EKey {
        E2EKey::new(0x1234, 0x5678)
    }

    #[test]
    fn register_and_check_profile4() {
        let mut reg: E2ERegistry = E2ERegistry::new();
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
        let (status, stripped) = reg.check(key, &out[..len], [0; 8]).unwrap();
        assert_eq!(status, E2ECheckStatus::Ok);
        assert_eq!(stripped, payload);
    }

    #[test]
    fn register_and_check_profile5() {
        let mut reg: E2ERegistry = E2ERegistry::new();
        let key = make_key();
        let config = Profile5Config::new(0x1234, 20, 15);
        reg.register(key, E2EProfile::Profile5(config))
            .expect("register fits within E2E_REGISTRY_CAP");

        let mut payload = [0u8; 20];
        payload[..5].copy_from_slice(b"Hello");
        let mut out = [0u8; 64];
        let len = reg
            .protect(key, &payload, [0; 8], &mut out)
            .unwrap()
            .unwrap();

        let (status, stripped) = reg.check(key, &out[..len], [0; 8]).unwrap();
        assert_eq!(status, E2ECheckStatus::Ok);
        assert_eq!(stripped, &payload);
    }

    #[test]
    fn unregistered_key_returns_none() {
        let mut reg: E2ERegistry = E2ERegistry::new();
        let key = make_key();
        assert!(!reg.contains_key(&key));
        assert!(reg.check(key, b"test", [0; 8]).is_none());
        assert!(reg.protect(key, b"test", [0; 8], &mut [0; 64]).is_none());
    }

    #[test]
    fn unregister_removes_key() {
        let mut reg: E2ERegistry = E2ERegistry::new();
        let key = make_key();
        reg.register(key, E2EProfile::Profile4(Profile4Config::new(0, 15)))
            .expect("register fits within E2E_REGISTRY_CAP");
        assert!(reg.contains_key(&key));
        reg.unregister(&key);
        assert!(!reg.contains_key(&key));
    }

    #[test]
    fn default_is_empty() {
        let reg: E2ERegistry = E2ERegistry::default();
        assert!(!reg.contains_key(&make_key()));
    }

    /// Replacing the profile of an already-registered key MUST succeed
    /// even when the registry is at capacity — the slot is reused, not
    /// added. Regression guard for the FnvIndexMap "full + missing key"
    /// branch.
    #[test]
    fn register_replacement_succeeds_when_full() {
        let mut reg: E2ERegistry = E2ERegistry::new();
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
        let mut reg: E2ERegistry = E2ERegistry::new();
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
