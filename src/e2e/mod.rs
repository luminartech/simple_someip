//! SOME/IP-specific keying for the `simple_e2e` registry.
//!
//! Following the simple_e2e migration (0.8.0), this module is a thin
//! shim over `simple_e2e::registry`. The only SOME/IP-flavored type is
//! [`E2EKey`] — service+method-or-event keying derived from a SOME/IP
//! [`MessageId`](crate::protocol::MessageId). Everything else is
//! re-exported from `simple_e2e`.
//!
//! # Example
//!
//! ```ignore
//! use simple_someip::e2e::{
//!     E2ERegistry, E2EProfile, E2EKey, Profile4Config,
//! };
//!
//! let mut registry = E2ERegistry::new();
//! let key = E2EKey::new(0x1234, 0x5678);
//! let config = Profile4Config::new(0x12345678, 15);
//! registry
//!     .register(key, E2EProfile::Profile4(config))
//!     .expect("registry not full");
//!
//! let payload = b"Hello, SOME/IP!";
//! let mut buf = [0u8; 128];
//! let len = registry
//!     .protect(key, payload, [0; 8], &mut buf)
//!     .expect("key registered")
//!     .expect("buffer large enough");
//!
//! let outcome = registry
//!     .check(key, &buf[..len], [0; 8])
//!     .expect("key registered");
//! assert!(outcome.is_ok());
//! ```

pub use simple_e2e::profile4::{
    Config as Profile4Config, HEADER_SIZE as PROFILE4_HEADER_SIZE, State as Profile4State,
};
pub use simple_e2e::profile5::{
    Config as Profile5Config, HEADER_SIZE as PROFILE5_HEADER_SIZE, State as Profile5State,
};
pub use simple_e2e::registry::{
    CheckOutcome, IncludeUpperHeader, Profile as E2EProfile, ProtectError as Error,
    RegistryFull as E2ERegistryFull,
};

/// SOME/IP-specific lookup key for E2E configuration.
///
/// A `(service_id, method_or_event_id)` pair uniquely identifies a data
/// element on the SOME/IP wire, and that's exactly what's keyed in the
/// registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct E2EKey {
    /// SOME/IP service ID.
    pub service_id: u16,
    /// SOME/IP method or event ID.
    pub method_or_event_id: u16,
}

impl E2EKey {
    /// Create a new key from explicit service and method/event IDs.
    #[must_use]
    pub const fn new(service_id: u16, method_or_event_id: u16) -> Self {
        Self {
            service_id,
            method_or_event_id,
        }
    }

    /// Derive a key from a [`MessageId`](crate::protocol::MessageId).
    #[must_use]
    pub fn from_message_id(message_id: crate::protocol::MessageId) -> Self {
        Self {
            service_id: message_id.service_id(),
            method_or_event_id: message_id.method_id(),
        }
    }
}

/// E2E registry pre-bound to [`E2EKey`] with capacity 32 — sized for
/// typical SOME/IP workloads (a service instance with up to a few dozen
/// E2E-protected message types).
pub type E2ERegistry = simple_e2e::registry::Registry<E2EKey, 32>;

/// SOME/IP-specific owned snapshot of a check outcome.
///
/// `simple_e2e::registry::CheckOutcome` borrows the input buffer for the
/// stripped-payload slice, which makes it unsuitable for cross-task
/// storage on `ReceivedMessage`. This owned variant captures just the
/// status half — profile-discriminated, no payload — so it can travel
/// through async channels.
///
/// Construct via [`Self::from_outcome`] right after `registry.check(...)`
/// returns, while the borrowed payload is still in scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckStatus {
    /// Profile 4 result.
    Profile4(simple_e2e::profile4::CheckStatus),
    /// Profile 5 result (covers both plain and with-upper-header bindings).
    Profile5(simple_e2e::profile5::CheckStatus),
}

impl CheckStatus {
    /// Copy the status half of a [`CheckOutcome`] into the owned form.
    #[must_use]
    pub fn from_outcome(outcome: &CheckOutcome<'_>) -> Self {
        match outcome {
            CheckOutcome::Profile4 { status, .. } => Self::Profile4(status.clone()),
            CheckOutcome::Profile5 { status, .. } => Self::Profile5(status.clone()),
        }
    }

    /// `true` only for status `Ok` — not `OkSomeLost`, `Repeated`,
    /// `WrongSequence`, or `Invalid`. Use for "did exactly the next-in-
    /// sequence valid message arrive?" checks.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        match self {
            Self::Profile4(s) => matches!(s, simple_e2e::profile4::CheckStatus::Ok),
            Self::Profile5(s) => matches!(s, simple_e2e::profile5::CheckStatus::Ok),
        }
    }

    /// Map to the AUTOSAR E2E return code byte used on the SOME/IP wire.
    ///
    /// `Ok` → 1, `OkSomeLost` → 4, `Repeated` → 3, `WrongSequence` → 5.
    /// `Invalid(CrcMismatch)` → 2 (CrcError); other `Invalid(_)` →
    /// 6 (BadArgument). 0 (Unchecked) is reserved for the "no profile
    /// registered" case the caller signals separately by passing `None`.
    #[must_use]
    pub fn to_return_code(&self) -> u8 {
        match self {
            Self::Profile4(s) => match s {
                simple_e2e::profile4::CheckStatus::Ok => 1,
                simple_e2e::profile4::CheckStatus::OkSomeLost => 4,
                simple_e2e::profile4::CheckStatus::Repeated => 3,
                simple_e2e::profile4::CheckStatus::WrongSequence => 5,
                simple_e2e::profile4::CheckStatus::Invalid(
                    simple_e2e::profile4::ValidateError::CrcMismatch { .. },
                ) => 2,
                simple_e2e::profile4::CheckStatus::Invalid(_) => 6,
            },
            Self::Profile5(s) => match s {
                simple_e2e::profile5::CheckStatus::Ok => 1,
                simple_e2e::profile5::CheckStatus::OkSomeLost => 4,
                simple_e2e::profile5::CheckStatus::Repeated => 3,
                simple_e2e::profile5::CheckStatus::WrongSequence => 5,
                simple_e2e::profile5::CheckStatus::Invalid(
                    simple_e2e::profile5::ValidateError::CrcMismatch { .. },
                ) => 2,
                simple_e2e::profile5::CheckStatus::Invalid(_) => 6,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn e2e_key_from_message_id() {
        let message_id = crate::protocol::MessageId::new_from_service_and_method(0x1234, 0x5678);
        let key = E2EKey::from_message_id(message_id);
        assert_eq!(key.service_id, 0x1234);
        assert_eq!(key.method_or_event_id, 0x5678);
    }

    #[test]
    fn registry_round_trip_profile4() {
        let mut registry = E2ERegistry::new();
        let key = E2EKey::new(0x1234, 0x5678);
        registry
            .register(
                key,
                E2EProfile::Profile4(Profile4Config::new(0x12345678, 15)),
            )
            .expect("register fits within capacity");

        let payload = b"hello";
        let mut buf = [0u8; 64];
        let len = registry
            .protect(key, payload, [0; 8], &mut buf)
            .expect("registered")
            .expect("buffer large");

        let outcome = registry
            .check(key, &buf[..len], [0; 8])
            .expect("registered");
        assert!(outcome.is_ok());
    }

    #[test]
    fn check_status_to_return_code_ok() {
        let status = CheckStatus::Profile4(simple_e2e::profile4::CheckStatus::Ok);
        assert_eq!(status.to_return_code(), 1);
    }

    #[test]
    fn check_status_to_return_code_crc_mismatch() {
        let status = CheckStatus::Profile4(simple_e2e::profile4::CheckStatus::Invalid(
            simple_e2e::profile4::ValidateError::CrcMismatch {
                got: 0xDEAD_BEEF,
                expected: 0xCAFE_BABE,
            },
        ));
        assert_eq!(status.to_return_code(), 2);
    }

    #[test]
    fn check_status_to_return_code_bad_argument() {
        // TooShort / LengthMismatch / DataIdMismatch all collapse to 6.
        let too_short = CheckStatus::Profile4(simple_e2e::profile4::CheckStatus::Invalid(
            simple_e2e::profile4::ValidateError::TooShort { actual: 5 },
        ));
        assert_eq!(too_short.to_return_code(), 6);
    }
}
