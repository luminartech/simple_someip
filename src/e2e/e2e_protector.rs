//! E2E protection functions for adding E2E headers to payloads.

use super::config::{Profile4Config, Profile5Config};
use super::crc::{compute_crc16_p5, compute_crc32_p4};
use super::state::{Profile4State, Profile5State};

/// Profile 4 header size in bytes.
pub const PROFILE4_HEADER_SIZE: usize = 12;

/// Profile 5 header size in bytes.
pub const PROFILE5_HEADER_SIZE: usize = 3;

/// Add E2E Profile 4 protection to a payload.
///
/// Creates a protected message with a 12-byte header prepended:
/// - Length (2 bytes): Total length including header
/// - Counter (2 bytes): Sequence counter from state
/// - DataID (4 bytes): From configuration
/// - CRC (4 bytes): CRC-32P4 over Length + Counter + DataID + Payload
///
/// The state counter is incremented after each call.
///
/// # Arguments
/// * `config` - Profile 4 configuration
/// * `state` - Mutable state for counter tracking
/// * `payload` - The payload data to protect
///
/// # Returns
/// A new Vec containing the E2E header followed by the payload.
pub fn protect_profile4(
    config: &Profile4Config,
    state: &mut Profile4State,
    payload: &[u8],
) -> Vec<u8> {
    let total_length = PROFILE4_HEADER_SIZE + payload.len();
    assert!(
        total_length <= u16::MAX as usize,
        "E2E Profile 4 payload too large: total length {} exceeds u16::MAX ({})",
        total_length,
        u16::MAX,
    );

    let counter = state.protect_counter;
    let length = total_length as u16;

    // Compute CRC over: Length + Counter + DataID + Payload
    let crc = compute_crc32_p4(length, counter, config.data_id, payload);

    // Build the protected message
    let mut result = Vec::with_capacity(PROFILE4_HEADER_SIZE + payload.len());

    // Header: Length (2) + Counter (2) + DataID (4) + CRC (4)
    result.extend_from_slice(&length.to_be_bytes());
    result.extend_from_slice(&counter.to_be_bytes());
    result.extend_from_slice(&config.data_id.to_be_bytes());
    result.extend_from_slice(&crc.to_be_bytes());

    // Payload
    result.extend_from_slice(payload);

    // Increment counter (wraps at u16::MAX)
    state.protect_counter = state.protect_counter.wrapping_add(1);

    result
}

/// Add E2E Profile 5 protection to a payload.
///
/// Creates a protected message with a 3-byte header prepended:
/// - CRC (2 bytes, little-endian): CRC-16-CCITT over Counter + Payload + DataID(LE)
/// - Counter (1 byte): Sequence counter from state
///
/// The state counter is incremented after each call.
///
/// # Arguments
/// * `config` - Profile 5 configuration
/// * `state` - Mutable state for counter tracking
/// * `payload` - The payload data to protect
///
/// # Returns
/// A new Vec containing the E2E header followed by the payload.
pub fn protect_profile5(
    config: &Profile5Config,
    state: &mut Profile5State,
    payload: &[u8],
) -> Vec<u8> {
    let counter = state.protect_counter;

    // Compute CRC over: Counter + Payload + DataID (LE)
    let crc = compute_crc16_p5(config.data_id, counter, payload);

    // Build the protected message
    let mut result = Vec::with_capacity(PROFILE5_HEADER_SIZE + payload.len());

    // Header: CRC (2, little-endian) + Counter (1)
    result.extend_from_slice(&crc.to_le_bytes());
    result.push(counter);

    // Payload
    result.extend_from_slice(payload);

    // Increment counter (wraps at u8::MAX)
    state.protect_counter = state.protect_counter.wrapping_add(1);

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_protect_profile4_header_format() {
        let config = Profile4Config::new(0x12345678, 15);
        let mut state = Profile4State::new();

        let payload = b"test";
        let protected = protect_profile4(&config, &mut state, payload);

        // Check total length
        assert_eq!(protected.len(), 12 + 4); // header + payload

        // Check length field (first 2 bytes)
        let length = u16::from_be_bytes([protected[0], protected[1]]);
        assert_eq!(length, 16); // 12 + 4

        // Check counter field (bytes 2-3)
        let counter = u16::from_be_bytes([protected[2], protected[3]]);
        assert_eq!(counter, 0);

        // Check data_id field (bytes 4-7)
        let data_id = u32::from_be_bytes([protected[4], protected[5], protected[6], protected[7]]);
        assert_eq!(data_id, 0x12345678);

        // Check payload at end
        assert_eq!(&protected[12..], b"test");
    }

    #[test]
    fn test_protect_profile4_counter_increment() {
        let config = Profile4Config::new(0x12345678, 15);
        let mut state = Profile4State::new();

        let payload = b"test";

        for i in 0..5 {
            let protected = protect_profile4(&config, &mut state, payload);
            let counter = u16::from_be_bytes([protected[2], protected[3]]);
            assert_eq!(counter, i);
        }
    }

    #[test]
    fn test_protect_profile4_counter_wraps() {
        let config = Profile4Config::new(0x12345678, 15);
        let mut state = Profile4State::with_initial_counter(u16::MAX);

        let payload = b"test";

        let protected1 = protect_profile4(&config, &mut state, payload);
        let counter1 = u16::from_be_bytes([protected1[2], protected1[3]]);
        assert_eq!(counter1, u16::MAX);

        let protected2 = protect_profile4(&config, &mut state, payload);
        let counter2 = u16::from_be_bytes([protected2[2], protected2[3]]);
        assert_eq!(counter2, 0); // Wrapped
    }

    #[test]
    fn test_protect_profile5_header_format() {
        let config = Profile5Config::new(0x1234, 20, 15);
        let mut state = Profile5State::new();

        let payload = b"test";
        let protected = protect_profile5(&config, &mut state, payload);

        // Check total length
        assert_eq!(protected.len(), 3 + 4); // header + payload

        // Header layout: [CRC_lo, CRC_hi, Counter]
        // Check counter field (third byte)
        assert_eq!(protected[2], 0);

        // Check payload at end
        assert_eq!(&protected[3..], b"test");
    }

    #[test]
    fn test_protect_profile5_counter_increment() {
        let config = Profile5Config::new(0x1234, 20, 15);
        let mut state = Profile5State::new();

        let payload = b"test";

        for i in 0..5u8 {
            let protected = protect_profile5(&config, &mut state, payload);
            assert_eq!(protected[2], i); // Counter is at byte 2
        }
    }

    #[test]
    fn test_protect_profile5_counter_wraps() {
        let config = Profile5Config::new(0x1234, 20, 15);
        let mut state = Profile5State::with_initial_counter(u8::MAX);

        let payload = b"test";

        let protected1 = protect_profile5(&config, &mut state, payload);
        assert_eq!(protected1[2], u8::MAX); // Counter is at byte 2

        let protected2 = protect_profile5(&config, &mut state, payload);
        assert_eq!(protected2[2], 0); // Wrapped
    }

    #[test]
    fn test_protect_profile4_empty_payload() {
        let config = Profile4Config::new(0x12345678, 15);
        let mut state = Profile4State::new();

        let protected = protect_profile4(&config, &mut state, b"");
        assert_eq!(protected.len(), 12); // Just header
    }

    #[test]
    fn test_protect_profile5_empty_payload() {
        let config = Profile5Config::new(0x1234, 3, 15);
        let mut state = Profile5State::new();

        let protected = protect_profile5(&config, &mut state, b"");
        assert_eq!(protected.len(), 3); // Just header
    }
}
