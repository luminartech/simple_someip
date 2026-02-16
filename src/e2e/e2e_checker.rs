//! E2E checking functions for validating E2E-protected payloads.

use super::config::{Profile4Config, Profile5Config};
use super::crc::{compute_crc16_p5, compute_crc32_p4};
use super::e2e_protector::{PROFILE4_HEADER_SIZE, PROFILE5_HEADER_SIZE};
use super::state::{Profile4State, Profile5State};
use super::{E2ECheckResult, E2ECheckStatus};

/// Check E2E Profile 4 protected data.
///
/// Validates the 12-byte header:
/// - Length (2 bytes): Verifies against actual message length
/// - Counter (2 bytes): Checks sequence continuity
/// - DataID (4 bytes): Must match configuration
/// - CRC (4 bytes): Verified against computed CRC-32P4
///
/// # Arguments
/// * `config` - Profile 4 configuration
/// * `state` - Mutable state for counter tracking
/// * `protected` - The protected message (header + payload)
///
/// # Returns
/// An E2ECheckResult containing the status, counter, and extracted payload.
pub fn check_profile4(
    config: &Profile4Config,
    state: &mut Profile4State,
    protected: &[u8],
) -> E2ECheckResult {
    // Check minimum length
    if protected.len() < PROFILE4_HEADER_SIZE {
        return E2ECheckResult::error(E2ECheckStatus::BadArgument);
    }

    // Parse header
    let length = u16::from_be_bytes([protected[0], protected[1]]);
    let counter = u16::from_be_bytes([protected[2], protected[3]]);
    let data_id = u32::from_be_bytes([protected[4], protected[5], protected[6], protected[7]]);
    let received_crc = u32::from_be_bytes([protected[8], protected[9], protected[10], protected[11]]);

    // Verify length field matches actual message length
    if length as usize != protected.len() {
        return E2ECheckResult::error(E2ECheckStatus::BadArgument);
    }

    // Verify DataID matches configuration
    if data_id != config.data_id {
        return E2ECheckResult::error(E2ECheckStatus::BadArgument);
    }

    // Extract payload
    let payload = &protected[PROFILE4_HEADER_SIZE..];

    // Compute and verify CRC
    let computed_crc = compute_crc32_p4(length, counter, data_id, payload);
    if computed_crc != received_crc {
        return E2ECheckResult::error(E2ECheckStatus::CrcError);
    }

    // Check sequence
    let status = check_sequence_profile4(state, counter, config.max_delta_counter);

    // Update state
    state.last_counter = Some(counter);

    E2ECheckResult::success(status, counter as u32, payload.to_vec())
}

/// Check E2E Profile 5 protected data.
///
/// Validates the 3-byte header:
/// - CRC (2 bytes, little-endian): Verified against computed CRC-16-CCITT
/// - Counter (1 byte): Checks sequence continuity
///
/// # Arguments
/// * `config` - Profile 5 configuration
/// * `state` - Mutable state for counter tracking
/// * `protected` - The protected message (header + payload)
///
/// # Returns
/// An E2ECheckResult containing the status, counter, and extracted payload.
pub fn check_profile5(
    config: &Profile5Config,
    state: &mut Profile5State,
    protected: &[u8],
) -> E2ECheckResult {
    // Check minimum length
    if protected.len() < PROFILE5_HEADER_SIZE {
        return E2ECheckResult::error(E2ECheckStatus::BadArgument);
    }

    // Verify data length matches configuration (header + payload = config.data_length)
    let expected_total_length = PROFILE5_HEADER_SIZE + config.data_length as usize;
    if protected.len() != expected_total_length {
        tracing::warn!(
            "E2E Profile 5 length mismatch: expected {} bytes (3 header + {} payload), got {} bytes",
            expected_total_length,
            config.data_length,
            protected.len()
        );
        return E2ECheckResult::error(E2ECheckStatus::BadArgument);
    }

    // Parse header: CRC (2, little-endian) + Counter (1)
    let received_crc = u16::from_le_bytes([protected[0], protected[1]]);
    let counter = protected[2];

    // Extract payload
    let payload = &protected[PROFILE5_HEADER_SIZE..];

    // Compute and verify CRC
    let computed_crc = compute_crc16_p5(config.data_id, counter, payload);
    if computed_crc != received_crc {
        return E2ECheckResult::error(E2ECheckStatus::CrcError);
    }

    // Check sequence
    let status = check_sequence_profile5(state, counter, config.max_delta_counter);

    // Update state
    state.last_counter = Some(counter);

    E2ECheckResult::success(status, counter as u32, payload.to_vec())
}

/// Check sequence continuity for Profile 4 (16-bit counter).
fn check_sequence_profile4(
    state: &Profile4State,
    received_counter: u16,
    max_delta: u16,
) -> E2ECheckStatus {
    match state.last_counter {
        None => {
            // First message received - always Ok
            E2ECheckStatus::Ok
        }
        Some(last_counter) => {
            // Calculate delta with wraparound handling
            let delta = received_counter.wrapping_sub(last_counter);

            if delta == 0 {
                // Same counter value - repeated message
                E2ECheckStatus::Repeated
            } else if delta == 1 {
                // Consecutive message - perfect
                E2ECheckStatus::Ok
            } else if delta <= max_delta {
                // Some messages lost but within tolerance
                E2ECheckStatus::OkSomeLost
            } else {
                // Too many messages lost or counter went backwards
                E2ECheckStatus::WrongSequence
            }
        }
    }
}

/// Check sequence continuity for Profile 5 (8-bit counter).
fn check_sequence_profile5(
    state: &Profile5State,
    received_counter: u8,
    max_delta: u8,
) -> E2ECheckStatus {
    match state.last_counter {
        None => {
            // First message received - always Ok
            E2ECheckStatus::Ok
        }
        Some(last_counter) => {
            // Calculate delta with wraparound handling
            let delta = received_counter.wrapping_sub(last_counter);

            if delta == 0 {
                // Same counter value - repeated message
                E2ECheckStatus::Repeated
            } else if delta == 1 {
                // Consecutive message - perfect
                E2ECheckStatus::Ok
            } else if delta <= max_delta {
                // Some messages lost but within tolerance
                E2ECheckStatus::OkSomeLost
            } else {
                // Too many messages lost or counter went backwards
                E2ECheckStatus::WrongSequence
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::e2e::{protect_profile4, protect_profile5};

    #[test]
    fn test_check_profile4_valid() {
        let config = Profile4Config::new(0x12345678, 15);
        let mut protect_state = Profile4State::new();
        let mut check_state = Profile4State::new();

        let payload = b"Hello, World!";
        let protected = protect_profile4(&config, &mut protect_state, payload);

        let result = check_profile4(&config, &mut check_state, &protected);
        assert_eq!(result.status, E2ECheckStatus::Ok);
        assert_eq!(result.counter, Some(0));
        assert_eq!(result.payload.as_deref(), Some(payload.as_slice()));
    }

    #[test]
    fn test_check_profile4_wrong_data_id() {
        let config1 = Profile4Config::new(0x12345678, 15);
        let config2 = Profile4Config::new(0xDEADBEEF, 15);
        let mut protect_state = Profile4State::new();
        let mut check_state = Profile4State::new();

        let payload = b"test";
        let protected = protect_profile4(&config1, &mut protect_state, payload);

        // Check with different data_id
        let result = check_profile4(&config2, &mut check_state, &protected);
        assert_eq!(result.status, E2ECheckStatus::BadArgument);
    }

    #[test]
    fn test_check_profile4_corrupted_crc() {
        let config = Profile4Config::new(0x12345678, 15);
        let mut protect_state = Profile4State::new();
        let mut check_state = Profile4State::new();

        let payload = b"test";
        let mut protected = protect_profile4(&config, &mut protect_state, payload);

        // Corrupt CRC (bytes 8-11)
        protected[8] ^= 0xFF;

        let result = check_profile4(&config, &mut check_state, &protected);
        assert_eq!(result.status, E2ECheckStatus::CrcError);
    }

    #[test]
    fn test_check_profile4_corrupted_payload() {
        let config = Profile4Config::new(0x12345678, 15);
        let mut protect_state = Profile4State::new();
        let mut check_state = Profile4State::new();

        let payload = b"test";
        let mut protected = protect_profile4(&config, &mut protect_state, payload);

        // Corrupt payload
        protected[12] ^= 0xFF;

        let result = check_profile4(&config, &mut check_state, &protected);
        assert_eq!(result.status, E2ECheckStatus::CrcError);
    }

    #[test]
    fn test_check_profile4_wrong_length() {
        let config = Profile4Config::new(0x12345678, 15);
        let mut protect_state = Profile4State::new();
        let mut check_state = Profile4State::new();

        let payload = b"test";
        let mut protected = protect_profile4(&config, &mut protect_state, payload);

        // Truncate message
        protected.truncate(14);

        let result = check_profile4(&config, &mut check_state, &protected);
        assert_eq!(result.status, E2ECheckStatus::BadArgument);
    }

    #[test]
    fn test_check_profile4_too_short() {
        let config = Profile4Config::new(0x12345678, 15);
        let mut check_state = Profile4State::new();

        let short = [0u8; 11]; // Less than 12-byte header
        let result = check_profile4(&config, &mut check_state, &short);
        assert_eq!(result.status, E2ECheckStatus::BadArgument);
    }

    #[test]
    fn test_check_profile5_valid() {
        let config = Profile5Config::new(0x1234, 20, 15);
        let mut protect_state = Profile5State::new();
        let mut check_state = Profile5State::new();

        // Payload must be padded to data_length (20 bytes) for check_profile5
        let mut payload = [0u8; 20];
        payload[..13].copy_from_slice(b"Hello, World!");
        let protected = protect_profile5(&config, &mut protect_state, &payload);

        let result = check_profile5(&config, &mut check_state, &protected);
        assert_eq!(result.status, E2ECheckStatus::Ok);
        assert_eq!(result.counter, Some(0));
        assert_eq!(result.payload.as_deref(), Some(payload.as_slice()));
    }

    #[test]
    fn test_check_profile5_corrupted_crc() {
        let config = Profile5Config::new(0x1234, 20, 15);
        let mut protect_state = Profile5State::new();
        let mut check_state = Profile5State::new();

        let mut payload = [0u8; 20];
        payload[..4].copy_from_slice(b"test");
        let mut protected = protect_profile5(&config, &mut protect_state, &payload);

        // Corrupt CRC (bytes 1-2)
        protected[1] ^= 0xFF;

        let result = check_profile5(&config, &mut check_state, &protected);
        assert_eq!(result.status, E2ECheckStatus::CrcError);
    }

    #[test]
    fn test_check_profile5_too_short() {
        let config = Profile5Config::new(0x1234, 20, 15);
        let mut check_state = Profile5State::new();

        let short = [0u8; 2]; // Less than 3-byte header
        let result = check_profile5(&config, &mut check_state, &short);
        assert_eq!(result.status, E2ECheckStatus::BadArgument);
    }

    #[test]
    fn test_sequence_repeated() {
        let config = Profile4Config::new(0x12345678, 15);
        let mut protect_state = Profile4State::new();
        let mut check_state = Profile4State::new();

        let payload = b"test";
        let protected = protect_profile4(&config, &mut protect_state, payload);

        // First check
        let result1 = check_profile4(&config, &mut check_state, &protected);
        assert_eq!(result1.status, E2ECheckStatus::Ok);

        // Replay same message
        let result2 = check_profile4(&config, &mut check_state, &protected);
        assert_eq!(result2.status, E2ECheckStatus::Repeated);
    }

    #[test]
    fn test_sequence_consecutive() {
        let config = Profile4Config::new(0x12345678, 15);
        let mut protect_state = Profile4State::new();
        let mut check_state = Profile4State::new();

        let payload = b"test";

        for _ in 0..5 {
            let protected = protect_profile4(&config, &mut protect_state, payload);
            let result = check_profile4(&config, &mut check_state, &protected);
            assert_eq!(result.status, E2ECheckStatus::Ok);
        }
    }

    #[test]
    fn test_sequence_some_lost() {
        let config = Profile4Config::new(0x12345678, 10);
        let mut protect_state = Profile4State::new();
        let mut check_state = Profile4State::new();

        let payload = b"test";

        // First message
        let protected1 = protect_profile4(&config, &mut protect_state, payload);
        let result1 = check_profile4(&config, &mut check_state, &protected1);
        assert_eq!(result1.status, E2ECheckStatus::Ok);

        // Skip some messages
        for _ in 0..5 {
            let _ = protect_profile4(&config, &mut protect_state, payload);
        }

        // Check with gap of 6 (within max_delta of 10)
        let protected2 = protect_profile4(&config, &mut protect_state, payload);
        let result2 = check_profile4(&config, &mut check_state, &protected2);
        assert_eq!(result2.status, E2ECheckStatus::OkSomeLost);
    }

    #[test]
    fn test_sequence_wrong_sequence() {
        let config = Profile4Config::new(0x12345678, 3);
        let mut protect_state = Profile4State::new();
        let mut check_state = Profile4State::new();

        let payload = b"test";

        // First message
        let protected1 = protect_profile4(&config, &mut protect_state, payload);
        let result1 = check_profile4(&config, &mut check_state, &protected1);
        assert_eq!(result1.status, E2ECheckStatus::Ok);

        // Skip many messages
        for _ in 0..10 {
            let _ = protect_profile4(&config, &mut protect_state, payload);
        }

        // Check with gap of 11 (exceeds max_delta of 3)
        let protected2 = protect_profile4(&config, &mut protect_state, payload);
        let result2 = check_profile4(&config, &mut check_state, &protected2);
        assert_eq!(result2.status, E2ECheckStatus::WrongSequence);
    }

    #[test]
    fn test_sequence_wraparound() {
        let config = Profile4Config::new(0x12345678, 5);
        let mut protect_state = Profile4State::with_initial_counter(u16::MAX - 2);
        let mut check_state = Profile4State::new();

        let payload = b"test";

        // Messages around counter wraparound
        for _ in 0..5 {
            let protected = protect_profile4(&config, &mut protect_state, payload);
            let result = check_profile4(&config, &mut check_state, &protected);
            assert_eq!(result.status, E2ECheckStatus::Ok);
        }
    }

    #[test]
    fn test_profile5_sequence_wraparound() {
        let config = Profile5Config::new(0x1234, 20, 5);
        let mut protect_state = Profile5State::with_initial_counter(u8::MAX - 2);
        let mut check_state = Profile5State::new();

        let mut payload = [0u8; 20];
        payload[..4].copy_from_slice(b"test");

        // Messages around counter wraparound
        for _ in 0..5 {
            let protected = protect_profile5(&config, &mut protect_state, &payload);
            let result = check_profile5(&config, &mut check_state, &protected);
            assert_eq!(result.status, E2ECheckStatus::Ok);
        }
    }
}
