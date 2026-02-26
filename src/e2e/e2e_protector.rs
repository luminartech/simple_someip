//! E2E protection functions for adding E2E headers to payloads.

use super::config::{Profile4Config, Profile5Config};
use super::crc::{compute_crc16_p5, compute_crc16_p5_with_header, compute_crc32_p4};
use super::state::{Profile4State, Profile5State};
use crate::Error;

/// Profile 4 header size in bytes.
pub const PROFILE4_HEADER_SIZE: usize = 12;

/// Profile 5 header size in bytes.
pub const PROFILE5_HEADER_SIZE: usize = 3;

/// Add E2E Profile 4 protection to a payload.
///
/// Writes a protected message into `output` with a 12-byte header prepended:
/// - Length (2 bytes): Total length including header
/// - Counter (2 bytes): Sequence counter from state
/// - `DataID` (4 bytes): From configuration
/// - CRC (4 bytes): CRC-32P4 over Length + Counter + `DataID` + Payload
///
/// The state counter is incremented after each call.
///
/// # Arguments
/// * `config` - Profile 4 configuration
/// * `state` - Mutable state for counter tracking
/// * `payload` - The payload data to protect
/// * `output` - Buffer to write the protected message into; must be at least
///   `PROFILE4_HEADER_SIZE + payload.len()` bytes
///
/// # Returns
/// The number of bytes written to `output`, or an error if the buffer is too
/// small.
///
/// # Errors
/// Returns [`Error::BufferTooSmall`] if `output` is too small to hold the
/// protected message, or if the total message length (header + payload)
/// exceeds 65 535 bytes (the Profile 4 length field is `u16`).
pub fn protect_profile4(
    config: &Profile4Config,
    state: &mut Profile4State,
    payload: &[u8],
    output: &mut [u8],
) -> Result<usize, Error> {
    let total_length = PROFILE4_HEADER_SIZE + payload.len();

    if output.len() < total_length {
        return Err(Error::BufferTooSmall {
            needed: total_length,
            actual: output.len(),
        });
    }

    // Profile 4 length field is u16; if total_length > u16::MAX the buffer
    // requirement already exceeds what the protocol can encode, so report it
    // as a buffer-too-small error (needed would be > 65535).
    let length = u16::try_from(total_length).map_err(|_| Error::BufferTooSmall {
        needed: total_length,
        actual: output.len(),
    })?;

    let counter = state.protect_counter;

    // Compute CRC over: Length + Counter + DataID + Payload
    let crc = compute_crc32_p4(length, counter, config.data_id, payload);

    // Header: Length (2) + Counter (2) + DataID (4) + CRC (4)
    output[0..2].copy_from_slice(&length.to_be_bytes());
    output[2..4].copy_from_slice(&counter.to_be_bytes());
    output[4..8].copy_from_slice(&config.data_id.to_be_bytes());
    output[8..12].copy_from_slice(&crc.to_be_bytes());

    // Payload
    output[PROFILE4_HEADER_SIZE..total_length].copy_from_slice(payload);

    // Increment counter (wraps at u16::MAX)
    state.protect_counter = state.protect_counter.wrapping_add(1);

    Ok(total_length)
}

/// Add E2E Profile 5 protection to a payload.
///
/// Writes a protected message into `output` with a 3-byte header prepended:
/// - CRC (2 bytes, little-endian): CRC-16-CCITT over Counter + Payload + DataID(LE)
/// - Counter (1 byte): Sequence counter from state
///
/// The state counter is incremented after each call.
///
/// # Arguments
/// * `config` - Profile 5 configuration
/// * `state` - Mutable state for counter tracking
/// * `payload` - The payload data to protect
/// * `output` - Buffer to write the protected message into; must be at least
///   `PROFILE5_HEADER_SIZE + payload.len()` bytes
///
/// # Returns
/// The number of bytes written to `output`, or an error if the buffer is too
/// small.
///
/// # Errors
/// Returns [`Error::BufferTooSmall`] if `output` is too small to hold the
/// protected message.
pub fn protect_profile5(
    config: &Profile5Config,
    state: &mut Profile5State,
    payload: &[u8],
    output: &mut [u8],
) -> Result<usize, Error> {
    let total_length = PROFILE5_HEADER_SIZE + payload.len();

    if output.len() < total_length {
        return Err(Error::BufferTooSmall {
            needed: total_length,
            actual: output.len(),
        });
    }

    let counter = state.protect_counter;

    // Compute CRC over: Counter + Payload + DataID (LE)
    let crc = compute_crc16_p5(config.data_id, counter, payload);

    // Header: CRC (2, little-endian) + Counter (1)
    output[0..2].copy_from_slice(&crc.to_le_bytes());
    output[2] = counter;

    // Payload
    output[PROFILE5_HEADER_SIZE..total_length].copy_from_slice(payload);

    // Increment counter (wraps at u8::MAX)
    state.protect_counter = state.protect_counter.wrapping_add(1);

    Ok(total_length)
}

/// Add E2E Profile 5 protection with SOME/IP upper-header in the CRC.
///
/// Creates a protected message with a 3-byte header prepended:
/// - CRC (2 bytes, little-endian): CRC-16-CCITT over
///   `upper_header(8) + Counter(1) + Payload(N) + DataID(2 LE)`
/// - Counter (1 byte): Sequence counter from state
///
/// The 8-byte `upper_header` (UPPER-HEADER-BITS-TO-SHIFT = 64 bits) is the
/// second half of the SOME/IP header: `[request_id:4 BE, proto_ver:1,
/// iface_ver:1, msg_type:1, return_code:1]`. The state counter is incremented
/// after each call.
///
/// # Arguments
/// * `config` - Profile 5 configuration (data ID, data length, max delta counter)
/// * `state` - Mutable state for counter tracking
/// * `payload` - The payload data to protect
/// * `upper_header` - 8-byte SOME/IP upper header included in the CRC
///
/// # Returns
/// A new `Vec` containing the 3-byte E2E header followed by the payload.
pub fn protect_profile5_with_header(
    config: &Profile5Config,
    state: &mut Profile5State,
    payload: &[u8],
    upper_header: [u8; 8],
) -> Vec<u8> {
    let counter = state.protect_counter;
    let crc = compute_crc16_p5_with_header(config.data_id, counter, payload, upper_header);

    let mut result = Vec::with_capacity(PROFILE5_HEADER_SIZE + payload.len());
    result.extend_from_slice(&crc.to_le_bytes());
    result.push(counter);
    result.extend_from_slice(payload);

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
        let mut buf = [0u8; 256];
        let len = protect_profile4(&config, &mut state, payload, &mut buf).unwrap();
        let protected = &buf[..len];

        // Check total length
        assert_eq!(len, 12 + 4); // header + payload

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
        let mut buf = [0u8; 256];

        for i in 0..5 {
            let len = protect_profile4(&config, &mut state, payload, &mut buf).unwrap();
            let counter = u16::from_be_bytes([buf[2], buf[3]]);
            assert_eq!(counter, i);
            assert_eq!(len, 16);
        }
    }

    #[test]
    fn test_protect_profile4_counter_wraps() {
        let config = Profile4Config::new(0x12345678, 15);
        let mut state = Profile4State::with_initial_counter(u16::MAX);

        let payload = b"test";
        let mut buf = [0u8; 256];

        protect_profile4(&config, &mut state, payload, &mut buf).unwrap();
        let counter1 = u16::from_be_bytes([buf[2], buf[3]]);
        assert_eq!(counter1, u16::MAX);

        protect_profile4(&config, &mut state, payload, &mut buf).unwrap();
        let counter2 = u16::from_be_bytes([buf[2], buf[3]]);
        assert_eq!(counter2, 0); // Wrapped
    }

    #[test]
    fn test_protect_profile5_header_format() {
        let config = Profile5Config::new(0x1234, 20, 15);
        let mut state = Profile5State::new();

        let payload = b"test";
        let mut buf = [0u8; 256];
        let len = protect_profile5(&config, &mut state, payload, &mut buf).unwrap();
        let protected = &buf[..len];

        // Check total length
        assert_eq!(len, 3 + 4); // header + payload

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
        let mut buf = [0u8; 256];

        for i in 0..5u8 {
            protect_profile5(&config, &mut state, payload, &mut buf).unwrap();
            assert_eq!(buf[2], i); // Counter is at byte 2
        }
    }

    #[test]
    fn test_protect_profile5_counter_wraps() {
        let config = Profile5Config::new(0x1234, 20, 15);
        let mut state = Profile5State::with_initial_counter(u8::MAX);

        let payload = b"test";
        let mut buf = [0u8; 256];

        protect_profile5(&config, &mut state, payload, &mut buf).unwrap();
        assert_eq!(buf[2], u8::MAX); // Counter is at byte 2

        protect_profile5(&config, &mut state, payload, &mut buf).unwrap();
        assert_eq!(buf[2], 0); // Wrapped
    }

    #[test]
    fn test_protect_profile5_with_header_format() {
        let config = Profile5Config::new(0x1234, 20, 15);
        let mut state = Profile5State::new();

        let payload = b"test";
        let upper_header: [u8; 8] = [0x00, 0x01, 0x00, 0x05, 0x01, 0x03, 0x02, 0x00];
        let protected = protect_profile5_with_header(&config, &mut state, payload, upper_header);

        // Check total length
        assert_eq!(protected.len(), 3 + 4); // header + payload

        // Check counter field (third byte)
        assert_eq!(protected[2], 0);

        // Check payload at end
        assert_eq!(&protected[3..], b"test");
    }

    #[test]
    fn test_protect_profile5_with_header_counter_increment() {
        let config = Profile5Config::new(0x1234, 20, 15);
        let mut state = Profile5State::new();

        let payload = b"test";
        let upper_header: [u8; 8] = [0x00, 0x01, 0x00, 0x05, 0x01, 0x03, 0x02, 0x00];

        for i in 0..5u8 {
            let protected =
                protect_profile5_with_header(&config, &mut state, payload, upper_header);
            assert_eq!(protected[2], i);
        }
    }

    #[test]
    fn test_protect_profile5_with_header_counter_wraps() {
        let config = Profile5Config::new(0x1234, 20, 15);
        let mut state = Profile5State::with_initial_counter(u8::MAX);

        let payload = b"test";
        let upper_header: [u8; 8] = [0x00, 0x01, 0x00, 0x05, 0x01, 0x03, 0x02, 0x00];

        let protected = protect_profile5_with_header(&config, &mut state, payload, upper_header);
        assert_eq!(protected[2], u8::MAX);

        let protected = protect_profile5_with_header(&config, &mut state, payload, upper_header);
        assert_eq!(protected[2], 0); // Wrapped
    }

    #[test]
    fn test_protect_profile5_with_header_empty_payload() {
        let config = Profile5Config::new(0x1234, 3, 15);
        let mut state = Profile5State::new();

        let upper_header: [u8; 8] = [0x00, 0x01, 0x00, 0x05, 0x01, 0x03, 0x02, 0x00];
        let protected = protect_profile5_with_header(&config, &mut state, b"", upper_header);
        assert_eq!(protected.len(), 3); // Just header
    }

    #[test]
    fn test_protect_profile5_with_header_differs_from_no_header() {
        let config = Profile5Config::new(0x1234, 20, 15);
        let mut state_a = Profile5State::new();
        let mut state_b = Profile5State::new();

        let payload = b"test";
        let upper_header: [u8; 8] = [0x00, 0x01, 0x00, 0x05, 0x01, 0x03, 0x02, 0x00];

        let mut buf = [0u8; 256];
        let len = protect_profile5(&config, &mut state_a, payload, &mut buf).unwrap();
        let without_header_crc = u16::from_le_bytes([buf[0], buf[1]]);

        let with_header =
            protect_profile5_with_header(&config, &mut state_b, payload, upper_header);
        let with_header_crc = u16::from_le_bytes([with_header[0], with_header[1]]);

        // Same counter and payload but different CRC due to upper_header
        assert_eq!(buf[2], with_header[2]); // same counter
        assert_eq!(&buf[3..len], &with_header[3..]); // same payload
        assert_ne!(without_header_crc, with_header_crc); // different CRC
    }

    #[test]
    fn test_protect_profile4_empty_payload() {
        let config = Profile4Config::new(0x12345678, 15);
        let mut state = Profile4State::new();

        let mut buf = [0u8; 256];
        let len = protect_profile4(&config, &mut state, b"", &mut buf).unwrap();
        assert_eq!(len, 12); // Just header
    }

    #[test]
    fn test_protect_profile5_empty_payload() {
        let config = Profile5Config::new(0x1234, 3, 15);
        let mut state = Profile5State::new();

        let mut buf = [0u8; 256];
        let len = protect_profile5(&config, &mut state, b"", &mut buf).unwrap();
        assert_eq!(len, 3); // Just header
    }
}
