//! CRC computation helpers for E2E profiles.

use crc::{Crc, CRC_16_IBM_3740, CRC_32_AUTOSAR};

/// CRC-32P4 algorithm used by E2E Profile 4.
/// Polynomial: 0xF4ACFB13 (AUTOSAR CRC-32)
const CRC32_P4: Crc<u32> = Crc::<u32>::new(&CRC_32_AUTOSAR);

/// CRC-16-CCITT algorithm used by E2E Profile 5.
/// Polynomial: 0x1021, Init: 0xFFFF (IBM 3740 variant, also known as CRC-16-CCITT-FALSE)
const CRC16_CCITT: Crc<u16> = Crc::<u16>::new(&CRC_16_IBM_3740);

/// Compute CRC-32P4 for Profile 4.
///
/// The CRC is computed over: Length (2) + Counter (2) + `DataID` (4) + Payload
/// Note: CRC field itself is not included in the calculation.
pub fn compute_crc32_p4(length: u16, counter: u16, data_id: u32, payload: &[u8]) -> u32 {
    let mut digest = CRC32_P4.digest();

    // Length (big-endian)
    digest.update(&length.to_be_bytes());

    // Counter (big-endian)
    digest.update(&counter.to_be_bytes());

    // DataID (big-endian)
    digest.update(&data_id.to_be_bytes());

    // Payload
    digest.update(payload);

    digest.finalize()
}

/// Compute CRC-16-CCITT for Profile 5.
///
/// Per AUTOSAR E2E Profile 5, the CRC is computed over all data bytes except the
/// CRC field itself, plus the `DataID`. Specifically:
/// - Counter (1 byte) + Payload (N bytes) + `DataID` (2 bytes, little-endian)
///
/// Note: CRC field itself is not included in the calculation.
/// Note: `DataLength` is NOT included in the CRC calculation.
pub fn compute_crc16_p5(data_id: u16, counter: u8, payload: &[u8]) -> u16 {
    tracing::trace!(
        "CRC-16 Profile5: data_id=0x{:04X}, counter={}, payload_len={}, payload={:02X?}",
        data_id,
        counter,
        payload.len(),
        payload
    );

    let mut digest = CRC16_CCITT.digest();

    // Counter (single byte)
    digest.update(&[counter]);

    // Payload
    digest.update(payload);

    // DataID (little-endian)
    let data_id_bytes = data_id.to_le_bytes();
    digest.update(&data_id_bytes);

    let crc = digest.finalize();
    tracing::trace!("CRC-16 Profile5: computed CRC = 0x{:04X} (bytes: {:02X?})", crc, crc.to_le_bytes());
    
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crc32_p4_basic() {
        // Basic smoke test - verify CRC changes with different inputs
        let crc1 = compute_crc32_p4(10, 0, 0x12345678, b"test");
        let crc2 = compute_crc32_p4(10, 1, 0x12345678, b"test");
        let crc3 = compute_crc32_p4(10, 0, 0x12345679, b"test");
        let crc4 = compute_crc32_p4(10, 0, 0x12345678, b"Test");

        assert_ne!(crc1, crc2, "Different counter should produce different CRC");
        assert_ne!(crc1, crc3, "Different data_id should produce different CRC");
        assert_ne!(crc1, crc4, "Different payload should produce different CRC");
    }

    #[test]
    fn test_crc16_p5_basic() {
        // Basic smoke test - verify CRC changes with different inputs
        let crc1 = compute_crc16_p5(0x1234, 0, b"test");
        let crc2 = compute_crc16_p5(0x1234, 1, b"test");
        let crc3 = compute_crc16_p5(0x1235, 0, b"test");
        let crc4 = compute_crc16_p5(0x1234, 0, b"Test");

        assert_ne!(crc1, crc2, "Different counter should produce different CRC");
        assert_ne!(crc1, crc3, "Different data_id should produce different CRC");
        assert_ne!(crc1, crc4, "Different payload should produce different CRC");
    }

    #[test]
    fn test_crc32_p4_deterministic() {
        // Same inputs should always produce same output
        let crc1 = compute_crc32_p4(20, 5, 0xABCDEF01, b"payload data");
        let crc2 = compute_crc32_p4(20, 5, 0xABCDEF01, b"payload data");
        assert_eq!(crc1, crc2);
    }

    #[test]
    fn test_crc16_p5_deterministic() {
        // Same inputs should always produce same output
        let crc1 = compute_crc16_p5(0xABCD, 5, b"payload data");
        let crc2 = compute_crc16_p5(0xABCD, 5, b"payload data");
        assert_eq!(crc1, crc2);
    }

    #[test]
    fn test_crc32_p4_empty_payload() {
        // Should work with empty payload
        let crc = compute_crc32_p4(8, 0, 0x12345678, b"");
        assert_ne!(crc, 0); // CRC should be non-trivial even for empty payload
    }

    #[test]
    fn test_crc16_p5_empty_payload() {
        // Should work with empty payload
        let crc = compute_crc16_p5(0x1234, 0, b"");
        assert_ne!(crc, 0); // CRC should be non-trivial even for empty payload
    }
}
