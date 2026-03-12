//! E2E (End-to-End) protection for SOME/IP payloads.
//!
//! This module implements E2E Profile 4 and Profile 5 protection as specified
//! in the [Open SOME/IP Specification](https://github.com/some-ip-com/open-someip-spec).
//!
//! # Example
//!
//! ```
//! use simple_someip::e2e::{
//!     Profile4Config, Profile4State,
//!     protect_profile4, check_profile4,
//!     E2ECheckStatus,
//! };
//!
//! let config = Profile4Config::new(0x12345678, 15);
//! let mut protect_state = Profile4State::new();
//! let mut check_state = Profile4State::new();
//!
//! let payload = b"Hello, SOME/IP!";
//! let mut buf = [0u8; 128];
//! let len = protect_profile4(&config, &mut protect_state, payload, &mut buf).unwrap();
//!
//! let result = check_profile4(&config, &mut check_state, &buf[..len]);
//! assert!(matches!(result.status, E2ECheckStatus::Ok));
//! ```

mod config;
mod crc;
mod e2e_checker;
mod e2e_protector;
mod error;
#[cfg(feature = "std")]
mod registry;
mod state;

pub use config::{Profile4Config, Profile5Config};
pub use e2e_checker::{check_profile4, check_profile5, check_profile5_with_header};
pub use e2e_protector::{
    PROFILE4_HEADER_SIZE, PROFILE5_HEADER_SIZE, protect_profile4, protect_profile5,
    protect_profile5_with_header,
};
pub use error::Error;
#[cfg(feature = "std")]
pub use registry::E2ERegistry;
pub use state::{Profile4State, Profile5State};

/// Status result from E2E check operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum E2ECheckStatus {
    /// Initial state, no check performed yet.
    Unchecked,
    /// Check passed successfully.
    Ok,
    /// CRC verification failed.
    CrcError,
    /// Counter value is repeated (same as last received).
    Repeated,
    /// Check passed but some messages were lost (counter gap within tolerance).
    OkSomeLost,
    /// Counter sequence error (gap exceeds `max_delta_counter`).
    WrongSequence,
    /// Invalid input arguments (e.g., message too short).
    BadArgument,
}

impl E2ECheckStatus {
    /// Convert to a numeric return code compatible with E2E.
    #[must_use]
    pub fn to_return_code(self) -> u8 {
        match self {
            E2ECheckStatus::Unchecked => 0,
            E2ECheckStatus::Ok => 1,
            E2ECheckStatus::CrcError => 2,
            E2ECheckStatus::Repeated => 3,
            E2ECheckStatus::OkSomeLost => 4,
            E2ECheckStatus::WrongSequence => 5,
            E2ECheckStatus::BadArgument => 6,
        }
    }
}

/// Result from an E2E check operation.
#[derive(Debug, Clone)]
pub struct E2ECheckResult<'a> {
    /// Status of the E2E check.
    pub status: E2ECheckStatus,
    /// Counter value extracted from the header (if parsing succeeded).
    pub counter: Option<u32>,
    /// Extracted payload without E2E header (if check succeeded).
    ///
    /// This is a borrowed subslice of the input `protected` buffer and is only
    /// valid as long as that buffer is kept alive.
    pub payload: Option<&'a [u8]>,
}

impl<'a> E2ECheckResult<'a> {
    pub(crate) fn error(status: E2ECheckStatus) -> Self {
        Self {
            status,
            counter: None,
            payload: None,
        }
    }

    pub(crate) fn success(status: E2ECheckStatus, counter: u32, payload: &'a [u8]) -> Self {
        Self {
            status,
            counter: Some(counter),
            payload: Some(payload),
        }
    }

    /// Copy the extracted payload into an owned `Vec<u8>`.
    ///
    /// Returns `None` if the check did not produce a payload (e.g. on error).
    #[cfg(feature = "std")]
    #[must_use]
    pub fn to_owned_payload(&self) -> Option<std::vec::Vec<u8>> {
        self.payload.map(<[u8]>::to_vec)
    }
}

/// Describes which E2E profile to apply for a given data element.
#[derive(Debug, Clone)]
pub enum E2EProfile {
    /// E2E Profile 4 (CRC-32, 12-byte header).
    Profile4(Profile4Config),
    /// E2E Profile 5 (CRC-16, 3-byte header, no upper-header in CRC).
    Profile5(Profile5Config),
    /// E2E Profile 5 with SOME/IP upper-header included in the CRC.
    Profile5WithHeader(Profile5Config),
}

/// Identifies a data element for E2E protection lookup.
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

/// Internal E2E state, one per registered key.
#[cfg(feature = "std")]
#[derive(Debug, Clone)]
pub(crate) enum E2EState {
    /// State for Profile 4.
    Profile4(Profile4State),
    /// State for Profile 5 (used by both `Profile5` and `Profile5WithHeader`).
    Profile5(Profile5State),
}

#[cfg(feature = "std")]
impl E2EState {
    pub(crate) fn from_profile(profile: &E2EProfile) -> Self {
        match profile {
            E2EProfile::Profile4(_) => Self::Profile4(Profile4State::new()),
            E2EProfile::Profile5(_) | E2EProfile::Profile5WithHeader(_) => {
                Self::Profile5(Profile5State::new())
            }
        }
    }
}

/// Run the appropriate E2E check for the given profile, returning the status
/// and the best available payload slice (stripped on success, original on error).
#[cfg(feature = "std")]
pub(crate) fn e2e_check<'a>(
    profile: &E2EProfile,
    state: &mut E2EState,
    payload: &'a [u8],
    upper_header: [u8; 8],
) -> (E2ECheckStatus, &'a [u8]) {
    let result = match (profile, state) {
        (E2EProfile::Profile4(config), E2EState::Profile4(st)) => {
            check_profile4(config, st, payload)
        }
        (E2EProfile::Profile5(config), E2EState::Profile5(st)) => {
            check_profile5(config, st, payload)
        }
        (E2EProfile::Profile5WithHeader(config), E2EState::Profile5(st)) => {
            check_profile5_with_header(config, st, payload, upper_header)
        }
        _ => return (E2ECheckStatus::BadArgument, payload),
    };
    let stripped = result.payload.unwrap_or(payload);
    (result.status, stripped)
}

/// Run the appropriate E2E protect for the given profile.
///
/// # Errors
///
/// Returns [`Error::BufferTooSmall`] if `output` cannot hold the protected payload.
#[cfg(feature = "std")]
pub(crate) fn e2e_protect(
    profile: &E2EProfile,
    state: &mut E2EState,
    payload: &[u8],
    upper_header: [u8; 8],
    output: &mut [u8],
) -> Result<usize, Error> {
    match (profile, state) {
        (E2EProfile::Profile4(config), E2EState::Profile4(st)) => {
            protect_profile4(config, st, payload, output)
        }
        (E2EProfile::Profile5(config), E2EState::Profile5(st)) => {
            protect_profile5(config, st, payload, output)
        }
        (E2EProfile::Profile5WithHeader(config), E2EState::Profile5(st)) => {
            protect_profile5_with_header(config, st, payload, upper_header, output)
        }
        _ => unreachable!("E2EState is always created from E2EProfile"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_return_codes() {
        assert_eq!(E2ECheckStatus::Unchecked.to_return_code(), 0);
        assert_eq!(E2ECheckStatus::Ok.to_return_code(), 1);
        assert_eq!(E2ECheckStatus::CrcError.to_return_code(), 2);
        assert_eq!(E2ECheckStatus::Repeated.to_return_code(), 3);
        assert_eq!(E2ECheckStatus::OkSomeLost.to_return_code(), 4);
        assert_eq!(E2ECheckStatus::WrongSequence.to_return_code(), 5);
        assert_eq!(E2ECheckStatus::BadArgument.to_return_code(), 6);
    }

    #[test]
    fn test_profile4_roundtrip() {
        let config = Profile4Config::new(0x12345678, 15);
        let mut protect_state = Profile4State::new();
        let mut check_state = Profile4State::new();

        let payload = b"Test payload data";
        let mut buf = [0u8; 256];
        let len = protect_profile4(&config, &mut protect_state, payload, &mut buf).unwrap();
        let protected = &buf[..len];

        assert_eq!(len, payload.len() + 12); // 12-byte header

        let result = check_profile4(&config, &mut check_state, protected);
        assert_eq!(result.status, E2ECheckStatus::Ok);
        assert_eq!(result.counter, Some(0));
        assert_eq!(result.payload, Some(payload.as_slice()));
    }

    #[test]
    fn test_profile5_roundtrip() {
        let config = Profile5Config::new(0x1234, 20, 15);
        let mut protect_state = Profile5State::new();
        let mut check_state = Profile5State::new();

        // Payload must be padded to data_length (20 bytes) for check_profile5
        let mut payload = [0u8; 20];
        payload[..17].copy_from_slice(b"Test payload data");
        let mut buf = [0u8; 256];
        let len = protect_profile5(&config, &mut protect_state, &payload, &mut buf).unwrap();
        let protected = &buf[..len];

        assert_eq!(len, payload.len() + 3); // 3-byte header

        let result = check_profile5(&config, &mut check_state, protected);
        assert_eq!(result.status, E2ECheckStatus::Ok);
        assert_eq!(result.counter, Some(0));
        assert_eq!(result.payload, Some(payload.as_slice()));
    }

    #[test]
    fn test_profile4_sequence_detection() {
        let config = Profile4Config::new(0x12345678, 5);
        let mut protect_state = Profile4State::new();
        let mut check_state = Profile4State::new();

        let payload = b"Test";
        let mut buf1 = [0u8; 256];
        let mut buf2 = [0u8; 256];

        // First message - should be Ok
        let len1 = protect_profile4(&config, &mut protect_state, payload, &mut buf1).unwrap();
        let result1 = check_profile4(&config, &mut check_state, &buf1[..len1]);
        assert_eq!(result1.status, E2ECheckStatus::Ok);

        // Second message - should be Ok
        let len2 = protect_profile4(&config, &mut protect_state, payload, &mut buf2).unwrap();
        let result2 = check_profile4(&config, &mut check_state, &buf2[..len2]);
        assert_eq!(result2.status, E2ECheckStatus::Ok);

        // Replay first message - should be Repeated or WrongSequence
        let result3 = check_profile4(&config, &mut check_state, &buf1[..len1]);
        assert!(matches!(
            result3.status,
            E2ECheckStatus::Repeated | E2ECheckStatus::WrongSequence
        ));
    }

    #[test]
    fn test_profile4_some_lost_detection() {
        let config = Profile4Config::new(0x12345678, 5);
        let mut protect_state = Profile4State::new();
        let mut check_state = Profile4State::new();

        let payload = b"Test";
        let mut buf = [0u8; 256];

        // First message
        let len = protect_profile4(&config, &mut protect_state, payload, &mut buf).unwrap();
        let result1 = check_profile4(&config, &mut check_state, &buf[..len]);
        assert_eq!(result1.status, E2ECheckStatus::Ok);

        // Skip a few messages by advancing protector counter
        protect_profile4(&config, &mut protect_state, payload, &mut buf).unwrap();
        protect_profile4(&config, &mut protect_state, payload, &mut buf).unwrap();
        let len = protect_profile4(&config, &mut protect_state, payload, &mut buf).unwrap();

        // Check skipped message - should be OkSomeLost (delta=3, within max_delta=5)
        let result4 = check_profile4(&config, &mut check_state, &buf[..len]);
        assert_eq!(result4.status, E2ECheckStatus::OkSomeLost);
    }

    #[test]
    fn test_profile4_wrong_sequence_detection() {
        let config = Profile4Config::new(0x12345678, 2);
        let mut protect_state = Profile4State::new();
        let mut check_state = Profile4State::new();

        let payload = b"Test";
        let mut buf = [0u8; 256];

        // First message
        let len = protect_profile4(&config, &mut protect_state, payload, &mut buf).unwrap();
        let result1 = check_profile4(&config, &mut check_state, &buf[..len]);
        assert_eq!(result1.status, E2ECheckStatus::Ok);

        // Skip many messages (exceed max_delta)
        for _ in 0..5 {
            protect_profile4(&config, &mut protect_state, payload, &mut buf).unwrap();
        }
        let len = protect_profile4(&config, &mut protect_state, payload, &mut buf).unwrap();

        // Check - should be WrongSequence (delta=6, exceeds max_delta=2)
        let result = check_profile4(&config, &mut check_state, &buf[..len]);
        assert_eq!(result.status, E2ECheckStatus::WrongSequence);
    }

    #[test]
    fn test_profile4_crc_error() {
        let config = Profile4Config::new(0x12345678, 15);
        let mut protect_state = Profile4State::new();
        let mut check_state = Profile4State::new();

        let payload = b"Test";
        let mut buf = [0u8; 256];
        let len = protect_profile4(&config, &mut protect_state, payload, &mut buf).unwrap();

        // Corrupt the CRC (last 4 bytes of header)
        buf[8] ^= 0xFF;

        let result = check_profile4(&config, &mut check_state, &buf[..len]);
        assert_eq!(result.status, E2ECheckStatus::CrcError);
    }

    #[test]
    fn test_profile5_crc_error() {
        let config = Profile5Config::new(0x1234, 20, 15);
        let mut protect_state = Profile5State::new();
        let mut check_state = Profile5State::new();

        let mut payload = [0u8; 20];
        payload[..4].copy_from_slice(b"Test");
        let mut buf = [0u8; 256];
        let len = protect_profile5(&config, &mut protect_state, &payload, &mut buf).unwrap();

        // Corrupt the CRC (bytes 1-2 of header)
        buf[1] ^= 0xFF;

        let result = check_profile5(&config, &mut check_state, &buf[..len]);
        assert_eq!(result.status, E2ECheckStatus::CrcError);
    }

    #[test]
    fn test_profile4_bad_argument_short_message() {
        let config = Profile4Config::new(0x12345678, 15);
        let mut check_state = Profile4State::new();

        // Message too short (less than 12-byte header)
        let short_message = [0u8; 8];
        let result = check_profile4(&config, &mut check_state, &short_message);
        assert_eq!(result.status, E2ECheckStatus::BadArgument);
    }

    #[test]
    fn test_profile5_bad_argument_short_message() {
        let config = Profile5Config::new(0x1234, 20, 15);
        let mut check_state = Profile5State::new();

        // Message too short (less than 3-byte header)
        let short_message = [0u8; 2];
        let result = check_profile5(&config, &mut check_state, &short_message);
        assert_eq!(result.status, E2ECheckStatus::BadArgument);
    }
}
