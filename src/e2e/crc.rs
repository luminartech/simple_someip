//! CRC computation helpers for E2E profiles.

use crc::{CRC_16_IBM_3740, CRC_32_AUTOSAR, Crc};

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
    tracing::trace!(
        "CRC-16 Profile5: computed CRC = 0x{:04X} (bytes: {:02X?})",
        crc,
        crc.to_le_bytes()
    );

    crc
}

/// Compute CRC-16-CCITT for Profile 5 with SOME/IP upper-header prefix.
///
/// The 8-byte upper header (UPPER-HEADER-BITS-TO-SHIFT = 64 bits) is prepended
/// to the CRC input before Counter + Payload + `DataID`.
///
/// CRC input order: `upper_header(8)` + Counter(1) + Payload(N) + DataID(2 LE)
pub fn compute_crc16_p5_with_header(
    data_id: u16,
    counter: u8,
    payload: &[u8],
    upper_header: [u8; 8],
) -> u16 {
    tracing::trace!(
        "CRC-16 Profile5 (with header): data_id=0x{:04X}, counter={}, payload_len={}, upper_header={:02X?}, payload={:02X?}",
        data_id,
        counter,
        payload.len(),
        upper_header,
        payload
    );

    let mut digest = CRC16_CCITT.digest();
    digest.update(&upper_header);
    digest.update(&[counter]);
    digest.update(payload);
    digest.update(&data_id.to_le_bytes());

    let crc = digest.finalize();
    tracing::trace!(
        "CRC-16 Profile5 (with header): computed CRC = 0x{:04X} (bytes: {:02X?})",
        crc,
        crc.to_le_bytes()
    );

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

    #[test]
    fn test_crc16_p5_with_header_nonzero_header_changes_crc() {
        // A non-zero upper header must produce a different CRC than the headerless variant
        let header = [0x00, 0x01, 0x00, 0x05, 0x01, 0x03, 0x02, 0x00];
        let crc_no_header = compute_crc16_p5(0x1234, 0, b"test");
        let crc_with_header = compute_crc16_p5_with_header(0x1234, 0, b"test", header);
        assert_ne!(
            crc_no_header, crc_with_header,
            "Non-zero upper_header should change CRC vs headerless path"
        );
    }

    #[test]
    fn test_crc16_p5_with_header_different_headers_differ() {
        let header_a = [0x00, 0x01, 0x00, 0x05, 0x01, 0x03, 0x02, 0x00];
        let header_b = [0x00, 0x02, 0x00, 0x05, 0x01, 0x03, 0x02, 0x00]; // single byte changed
        let crc_a = compute_crc16_p5_with_header(0x1234, 0, b"test", header_a);
        let crc_b = compute_crc16_p5_with_header(0x1234, 0, b"test", header_b);
        assert_ne!(
            crc_a, crc_b,
            "Different upper_header should produce different CRC"
        );
    }

    #[test]
    fn test_crc16_p5_with_header_deterministic() {
        let header = [0x00, 0x01, 0x00, 0x05, 0x01, 0x03, 0x02, 0x00];
        let crc1 = compute_crc16_p5_with_header(0x1234, 0, b"test", header);
        let crc2 = compute_crc16_p5_with_header(0x1234, 0, b"test", header);
        assert_eq!(crc1, crc2, "Same inputs should always produce same CRC");
    }

    #[test]
    fn test_crc16_p5_with_header_each_field_matters() {
        let header = [0x00, 0x01, 0x00, 0x05, 0x01, 0x03, 0x02, 0x00];
        let baseline = compute_crc16_p5_with_header(0x1234, 0, b"test", header);

        let crc_diff_data_id = compute_crc16_p5_with_header(0x1235, 0, b"test", header);
        let crc_diff_counter = compute_crc16_p5_with_header(0x1234, 1, b"test", header);
        let crc_diff_payload = compute_crc16_p5_with_header(0x1234, 0, b"Test", header);

        assert_ne!(
            baseline, crc_diff_data_id,
            "Different data_id should change CRC"
        );
        assert_ne!(
            baseline, crc_diff_counter,
            "Different counter should change CRC"
        );
        assert_ne!(
            baseline, crc_diff_payload,
            "Different payload should change CRC"
        );
    }
}
