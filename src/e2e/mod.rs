//! AUTOSAR E2E (End-to-End) protection for SOME/IP payloads.
//!
//! This module implements E2E Profile 4 and Profile 5 protection as specified
//! in the AUTOSAR E2E Protocol Specification.
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
//! let protected = protect_profile4(&config, &mut protect_state, payload);
//!
//! let result = check_profile4(&config, &mut check_state, &protected);
//! assert!(matches!(result.status, E2ECheckStatus::Ok));
//! ```

mod config;
mod crc;
mod e2e_checker;
mod e2e_protector;
mod state;

pub use config::{Profile4Config, Profile5Config};
pub use e2e_checker::{check_profile4, check_profile5};
pub use e2e_protector::{protect_profile4, protect_profile5};
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
    /// Counter sequence error (gap exceeds max_delta_counter).
    WrongSequence,
    /// Invalid input arguments (e.g., message too short).
    BadArgument,
}

impl E2ECheckStatus {
    /// Convert to a numeric return code compatible with AUTOSAR E2E.
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
pub struct E2ECheckResult {
    /// Status of the E2E check.
    pub status: E2ECheckStatus,
    /// Counter value extracted from the header (if parsing succeeded).
    pub counter: Option<u32>,
    /// Extracted payload without E2E header (if check succeeded).
    pub payload: Option<Vec<u8>>,
}

impl E2ECheckResult {
    pub(crate) fn error(status: E2ECheckStatus) -> Self {
        Self {
            status,
            counter: None,
            payload: None,
        }
    }

    pub(crate) fn success(status: E2ECheckStatus, counter: u32, payload: Vec<u8>) -> Self {
        Self {
            status,
            counter: Some(counter),
            payload: Some(payload),
        }
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
        let protected = protect_profile4(&config, &mut protect_state, payload);

        assert_eq!(protected.len(), payload.len() + 12); // 12-byte header

        let result = check_profile4(&config, &mut check_state, &protected);
        assert_eq!(result.status, E2ECheckStatus::Ok);
        assert_eq!(result.counter, Some(0));
        assert_eq!(result.payload.as_deref(), Some(payload.as_slice()));
    }

    #[test]
    fn test_profile5_roundtrip() {
        let config = Profile5Config::new(0x1234, 20, 15);
        let mut protect_state = Profile5State::new();
        let mut check_state = Profile5State::new();

        // Payload must be padded to data_length (20 bytes) for check_profile5
        let mut payload = [0u8; 20];
        payload[..17].copy_from_slice(b"Test payload data");
        let protected = protect_profile5(&config, &mut protect_state, &payload);

        assert_eq!(protected.len(), payload.len() + 3); // 3-byte header

        let result = check_profile5(&config, &mut check_state, &protected);
        assert_eq!(result.status, E2ECheckStatus::Ok);
        assert_eq!(result.counter, Some(0));
        assert_eq!(result.payload.as_deref(), Some(payload.as_slice()));
    }

    #[test]
    fn test_profile4_sequence_detection() {
        let config = Profile4Config::new(0x12345678, 5);
        let mut protect_state = Profile4State::new();
        let mut check_state = Profile4State::new();

        let payload = b"Test";

        // First message - should be Ok
        let protected1 = protect_profile4(&config, &mut protect_state, payload);
        let result1 = check_profile4(&config, &mut check_state, &protected1);
        assert_eq!(result1.status, E2ECheckStatus::Ok);

        // Second message - should be Ok
        let protected2 = protect_profile4(&config, &mut protect_state, payload);
        let result2 = check_profile4(&config, &mut check_state, &protected2);
        assert_eq!(result2.status, E2ECheckStatus::Ok);

        // Replay first message - should be Repeated or WrongSequence
        let result3 = check_profile4(&config, &mut check_state, &protected1);
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

        // First message
        let protected1 = protect_profile4(&config, &mut protect_state, payload);
        let result1 = check_profile4(&config, &mut check_state, &protected1);
        assert_eq!(result1.status, E2ECheckStatus::Ok);

        // Skip a few messages by advancing protector counter
        let _ = protect_profile4(&config, &mut protect_state, payload);
        let _ = protect_profile4(&config, &mut protect_state, payload);
        let protected4 = protect_profile4(&config, &mut protect_state, payload);

        // Check skipped message - should be OkSomeLost (delta=3, within max_delta=5)
        let result4 = check_profile4(&config, &mut check_state, &protected4);
        assert_eq!(result4.status, E2ECheckStatus::OkSomeLost);
    }

    #[test]
    fn test_profile4_wrong_sequence_detection() {
        let config = Profile4Config::new(0x12345678, 2);
        let mut protect_state = Profile4State::new();
        let mut check_state = Profile4State::new();

        let payload = b"Test";

        // First message
        let protected1 = protect_profile4(&config, &mut protect_state, payload);
        let result1 = check_profile4(&config, &mut check_state, &protected1);
        assert_eq!(result1.status, E2ECheckStatus::Ok);

        // Skip many messages (exceed max_delta)
        for _ in 0..5 {
            let _ = protect_profile4(&config, &mut protect_state, payload);
        }
        let protected_late = protect_profile4(&config, &mut protect_state, payload);

        // Check - should be WrongSequence (delta=6, exceeds max_delta=2)
        let result = check_profile4(&config, &mut check_state, &protected_late);
        assert_eq!(result.status, E2ECheckStatus::WrongSequence);
    }

    #[test]
    fn test_profile4_crc_error() {
        let config = Profile4Config::new(0x12345678, 15);
        let mut protect_state = Profile4State::new();
        let mut check_state = Profile4State::new();

        let payload = b"Test";
        let mut protected = protect_profile4(&config, &mut protect_state, payload);

        // Corrupt the CRC (last 4 bytes of header)
        protected[8] ^= 0xFF;

        let result = check_profile4(&config, &mut check_state, &protected);
        assert_eq!(result.status, E2ECheckStatus::CrcError);
    }

    #[test]
    fn test_profile5_crc_error() {
        let config = Profile5Config::new(0x1234, 20, 15);
        let mut protect_state = Profile5State::new();
        let mut check_state = Profile5State::new();

        let mut payload = [0u8; 20];
        payload[..4].copy_from_slice(b"Test");
        let mut protected = protect_profile5(&config, &mut protect_state, &payload);

        // Corrupt the CRC (bytes 1-2 of header)
        protected[1] ^= 0xFF;

        let result = check_profile5(&config, &mut check_state, &protected);
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
