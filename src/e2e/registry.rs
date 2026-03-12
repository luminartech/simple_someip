//! E2E configuration registry for runtime E2E management.

use std::collections::HashMap;

use super::{E2ECheckStatus, E2EKey, E2EProfile, E2EState, Error, e2e_check, e2e_protect};

/// Registry mapping message keys to E2E profile configurations and state.
#[derive(Debug)]
pub struct E2ERegistry {
    map: HashMap<E2EKey, (E2EProfile, E2EState)>,
}

impl E2ERegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    /// Register an E2E profile for the given key, creating fresh state.
    pub fn register(&mut self, key: E2EKey, profile: E2EProfile) {
        let state = E2EState::from_profile(&profile);
        self.map.insert(key, (profile, state));
    }

    /// Remove E2E configuration for the given key.
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

impl Default for E2ERegistry {
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
        let (status, stripped) = reg.check(key, &out[..len], [0; 8]).unwrap();
        assert_eq!(status, E2ECheckStatus::Ok);
        assert_eq!(stripped, payload);
    }

    #[test]
    fn register_and_check_profile5() {
        let mut reg = E2ERegistry::new();
        let key = make_key();
        let config = Profile5Config::new(0x1234, 20, 15);
        reg.register(key, E2EProfile::Profile5(config));

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
        let mut reg = E2ERegistry::new();
        let key = make_key();
        assert!(!reg.contains_key(&key));
        assert!(reg.check(key, b"test", [0; 8]).is_none());
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
