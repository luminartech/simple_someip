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
//! let config = Profile4Config::new(0x1234_5678, 15);
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

mod error;
mod registry;

pub use error::Error;
pub use registry::{E2E_REGISTRY_CAP, E2E_RX_STATE_CAP, E2ERegistry, E2ERegistryFull};

/// Profile 4 configuration (`data_id`, `max_delta_counter`).
pub type Profile4Config = simple_e2e::profile4::Config;
/// Profile 4 per-direction counter state.
pub type Profile4State = simple_e2e::profile4::State;
/// Profile 5 configuration (`data_id`, `data_length`, `max_delta_counter`).
pub type Profile5Config = simple_e2e::profile5::Config;
/// Profile 5 per-direction counter state.
pub type Profile5State = simple_e2e::profile5::State;
/// Profile 4 header length in bytes (length, counter, data ID, CRC-32).
pub const PROFILE4_HEADER_SIZE: usize = simple_e2e::profile4::HEADER_SIZE;
/// Profile 5 header length in bytes (CRC-16, counter).
pub const PROFILE5_HEADER_SIZE: usize = simple_e2e::profile5::HEADER_SIZE;

/// Check a Profile 4 protected message, tracking the counter in `state`.
pub fn check_profile4<'a>(
    config: &Profile4Config,
    state: &mut Profile4State,
    protected: &'a [u8],
) -> E2ECheckResult<'a> {
    let result = simple_e2e::profile4::check(config, state, protected);
    E2ECheckResult {
        status: result.status.into(),
        counter: result.counter.map(u32::from),
        payload: result.payload,
    }
}

/// Check a Profile 5 protected message, tracking the counter in `state`.
pub fn check_profile5<'a>(
    config: &Profile5Config,
    state: &mut Profile5State,
    protected: &'a [u8],
) -> E2ECheckResult<'a> {
    let result = simple_e2e::profile5::check(config, state, protected);
    E2ECheckResult {
        status: result.status.into(),
        counter: result.counter.map(u32::from),
        payload: result.payload,
    }
}

/// Check a Profile 5 protected message whose CRC also covers the 8-byte
/// SOME/IP upper header.
pub fn check_profile5_with_header<'a>(
    config: &Profile5Config,
    state: &mut Profile5State,
    protected: &'a [u8],
    upper_header: [u8; 8],
) -> E2ECheckResult<'a> {
    let result = simple_e2e::profile5::check_with_header(config, state, protected, upper_header);
    E2ECheckResult {
        status: result.status.into(),
        counter: result.counter.map(u32::from),
        payload: result.payload,
    }
}

/// Protect `payload` with a Profile 4 header into `output`; returns the
/// number of bytes written.
///
/// # Errors
/// [`Error::BufferTooSmall`] if `output` cannot hold header + payload.
pub fn protect_profile4(
    config: &Profile4Config,
    state: &mut Profile4State,
    payload: &[u8],
    output: &mut [u8],
) -> Result<usize, Error> {
    simple_e2e::profile4::protect(config, state, payload, output).map_err(Error::from)
}

/// Protect `payload` with a Profile 5 header into `output`; returns the
/// number of bytes written.
///
/// # Errors
/// [`Error::BufferTooSmall`] if `output` cannot hold header + payload.
pub fn protect_profile5(
    config: &Profile5Config,
    state: &mut Profile5State,
    payload: &[u8],
    output: &mut [u8],
) -> Result<usize, Error> {
    simple_e2e::profile5::protect(config, state, payload, output).map_err(Error::from)
}

/// Protect `payload` with a Profile 5 header whose CRC also covers the
/// 8-byte SOME/IP upper header.
///
/// # Errors
/// [`Error::BufferTooSmall`] if `output` cannot hold header + payload.
pub fn protect_profile5_with_header(
    config: &Profile5Config,
    state: &mut Profile5State,
    payload: &[u8],
    upper_header: [u8; 8],
    output: &mut [u8],
) -> Result<usize, Error> {
    simple_e2e::profile5::protect_with_header(config, state, payload, upper_header, output)
        .map_err(Error::from)
}

/// Status result from E2E check operations.
///
/// `Unchecked` is a registry-level outcome (no profile is registered for the
/// message's [`E2EKey`]); the remaining variants come from the profile
/// algorithm in `simple_e2e`. A wire-level rejection carries the concrete
/// [`E2EValidateError`], so a CRC mismatch reports the received and computed
/// values instead of a bare tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum E2ECheckStatus {
    /// No E2E profile is registered for this message; nothing was checked.
    Unchecked,
    /// Valid CRC and the counter advanced by exactly one (or first message).
    Ok,
    /// Valid CRC; some messages were lost, within `max_delta_counter`.
    OkSomeLost,
    /// Valid CRC; the counter did not advance (duplicate).
    Repeated,
    /// Valid CRC; the counter jumped past `max_delta_counter`.
    WrongSequence,
    /// The message failed wire-level validation (length, data ID, or CRC).
    Invalid(E2EValidateError),
}

/// Wire-level validation failure, tagged with the profile that produced it.
///
/// Profile 4 uses a CRC-32 and Profile 5 a CRC-16, so the two error types
/// differ in width; this enum keeps each profile's real values rather than
/// widening them into a lossy common shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum E2EValidateError {
    /// Profile 4 (CRC-32) validation failure.
    #[error("profile 4: {0}")]
    Profile4(simple_e2e::profile4::ValidateError),
    /// Profile 5 (CRC-16) validation failure.
    #[error("profile 5: {0}")]
    Profile5(simple_e2e::profile5::ValidateError),
}

impl E2EValidateError {
    /// `true` when the failure is a CRC mismatch (as opposed to a length or
    /// data-ID problem). Selects the wire return code in
    /// [`E2ECheckStatus::to_return_code`].
    #[must_use]
    pub const fn is_crc_mismatch(self) -> bool {
        matches!(
            self,
            Self::Profile4(simple_e2e::profile4::ValidateError::CrcMismatch { .. })
                | Self::Profile5(simple_e2e::profile5::ValidateError::CrcMismatch { .. })
        )
    }
}

impl E2ECheckStatus {
    /// Convert to a numeric return code.
    ///
    /// The codes are the pre-0.13 values: a CRC mismatch is `2` (formerly
    /// `CrcError`) and every other validation failure is `6` (formerly
    /// `BadArgument`), so the wire mapping is unchanged.
    #[must_use]
    pub fn to_return_code(self) -> u8 {
        match self {
            E2ECheckStatus::Unchecked => 0,
            E2ECheckStatus::Ok => 1,
            E2ECheckStatus::Invalid(err) if err.is_crc_mismatch() => 2,
            E2ECheckStatus::Repeated => 3,
            E2ECheckStatus::OkSomeLost => 4,
            E2ECheckStatus::WrongSequence => 5,
            E2ECheckStatus::Invalid(_) => 6,
        }
    }
}

impl From<simple_e2e::profile4::CheckStatus> for E2ECheckStatus {
    fn from(status: simple_e2e::profile4::CheckStatus) -> Self {
        use simple_e2e::profile4::CheckStatus as S;
        match status {
            S::Ok => Self::Ok,
            S::OkSomeLost => Self::OkSomeLost,
            S::Repeated => Self::Repeated,
            S::WrongSequence => Self::WrongSequence,
            S::Invalid(err) => Self::Invalid(E2EValidateError::Profile4(err)),
        }
    }
}

impl From<simple_e2e::profile5::CheckStatus> for E2ECheckStatus {
    fn from(status: simple_e2e::profile5::CheckStatus) -> Self {
        use simple_e2e::profile5::CheckStatus as S;
        match status {
            S::Ok => Self::Ok,
            S::OkSomeLost => Self::OkSomeLost,
            S::Repeated => Self::Repeated,
            S::WrongSequence => Self::WrongSequence,
            S::Invalid(err) => Self::Invalid(E2EValidateError::Profile5(err)),
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

impl E2ECheckResult<'_> {
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
#[derive(Debug, Clone)]
pub(crate) enum E2EState {
    /// State for Profile 4.
    Profile4(Profile4State),
    /// State for Profile 5 (used by both `Profile5` and `Profile5WithHeader`).
    Profile5(Profile5State),
}

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
        _ => unreachable!("E2EState is always created from E2EProfile"),
    };
    let stripped = result.payload.unwrap_or(payload);
    (result.status, stripped)
}

/// Run the appropriate E2E protect for the given profile.
///
/// # Errors
///
/// Returns [`Error::BufferTooSmall`] if `output` cannot hold the protected payload.
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

    use simple_e2e::{profile4, profile5};

    #[test]
    fn test_status_return_codes() {
        assert_eq!(E2ECheckStatus::Unchecked.to_return_code(), 0);
        assert_eq!(E2ECheckStatus::Ok.to_return_code(), 1);
        let p5_crc = E2ECheckStatus::Invalid(E2EValidateError::Profile5(
            profile5::ValidateError::CrcMismatch {
                got: 1,
                expected: 2,
            },
        ));
        assert_eq!(p5_crc.to_return_code(), 2);
        let p4_crc = E2ECheckStatus::Invalid(E2EValidateError::Profile4(
            profile4::ValidateError::CrcMismatch {
                got: 1,
                expected: 2,
            },
        ));
        assert_eq!(p4_crc.to_return_code(), 2);
        assert_eq!(E2ECheckStatus::Repeated.to_return_code(), 3);
        assert_eq!(E2ECheckStatus::OkSomeLost.to_return_code(), 4);
        assert_eq!(E2ECheckStatus::WrongSequence.to_return_code(), 5);
        // Every non-CRC validation failure is the wire's "bad argument".
        for err in [
            E2EValidateError::Profile5(profile5::ValidateError::TooShort { actual: 1 }),
            E2EValidateError::Profile5(profile5::ValidateError::LengthMismatch {
                expected: 4,
                actual: 3,
            }),
            E2EValidateError::Profile4(profile4::ValidateError::TooShort { actual: 1 }),
            E2EValidateError::Profile4(profile4::ValidateError::LengthMismatch {
                header_length: 4,
                actual: 3,
            }),
            E2EValidateError::Profile4(profile4::ValidateError::DataIdMismatch {
                got: 1,
                expected: 2,
            }),
        ] {
            assert_eq!(E2ECheckStatus::Invalid(err).to_return_code(), 6, "{err:?}");
        }
    }

    #[test]
    fn test_check_status_from_profile_status() {
        assert_eq!(
            E2ECheckStatus::from(profile4::CheckStatus::OkSomeLost),
            E2ECheckStatus::OkSomeLost
        );
        assert_eq!(
            E2ECheckStatus::from(profile5::CheckStatus::WrongSequence),
            E2ECheckStatus::WrongSequence
        );
        let e = profile5::ValidateError::CrcMismatch {
            got: 0xBEEF,
            expected: 0xCAFE,
        };
        assert_eq!(
            E2ECheckStatus::from(profile5::CheckStatus::Invalid(e)),
            E2ECheckStatus::Invalid(E2EValidateError::Profile5(e))
        );
    }

    #[test]
    fn test_check_status_is_copy() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<E2ECheckStatus>();
        assert_copy::<E2EValidateError>();
    }

    #[test]
    fn test_profile4_roundtrip() {
        let config = Profile4Config::new(0x1234_5678, 15);
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
        let config = Profile4Config::new(0x1234_5678, 5);
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
        let config = Profile4Config::new(0x1234_5678, 5);
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
        let config = Profile4Config::new(0x1234_5678, 2);
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
        let config = Profile4Config::new(0x1234_5678, 15);
        let mut protect_state = Profile4State::new();
        let mut check_state = Profile4State::new();

        let payload = b"Test";
        let mut buf = [0u8; 256];
        let len = protect_profile4(&config, &mut protect_state, payload, &mut buf).unwrap();

        // Corrupt the CRC (last 4 bytes of header)
        buf[8] ^= 0xFF;

        let result = check_profile4(&config, &mut check_state, &buf[..len]);
        let E2ECheckStatus::Invalid(E2EValidateError::Profile4(
            profile4::ValidateError::CrcMismatch { got, expected },
        )) = result.status
        else {
            panic!("expected a Profile 4 CRC mismatch, got {:?}", result.status);
        };
        // Profile 4 stores the CRC big-endian at bytes 8..12; we flipped byte 8.
        assert_eq!(got, u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]));
        assert_eq!(
            expected,
            u32::from_be_bytes([buf[8] ^ 0xFF, buf[9], buf[10], buf[11]])
        );
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
        let E2ECheckStatus::Invalid(E2EValidateError::Profile5(
            profile5::ValidateError::CrcMismatch { got, expected },
        )) = result.status
        else {
            panic!("expected a Profile 5 CRC mismatch, got {:?}", result.status);
        };
        // Profile 5 stores the CRC little-endian at bytes 0..2; we flipped byte 1.
        assert_eq!(got, u16::from_le_bytes([buf[0], buf[1]]));
        assert_eq!(expected, u16::from_le_bytes([buf[0], buf[1] ^ 0xFF]));
    }

    #[test]
    fn test_profile4_bad_argument_short_message() {
        let config = Profile4Config::new(0x1234_5678, 15);
        let mut check_state = Profile4State::new();

        // Message too short (less than 12-byte header)
        let short_message = [0u8; 8];
        let result = check_profile4(&config, &mut check_state, &short_message);
        assert_eq!(
            result.status,
            E2ECheckStatus::Invalid(E2EValidateError::Profile4(
                profile4::ValidateError::TooShort { actual: 8 }
            ))
        );
    }

    #[test]
    fn test_profile5_bad_argument_short_message() {
        let config = Profile5Config::new(0x1234, 20, 15);
        let mut check_state = Profile5State::new();

        // Message too short (less than 3-byte header)
        let short_message = [0u8; 2];
        let result = check_profile5(&config, &mut check_state, &short_message);
        assert_eq!(
            result.status,
            E2ECheckStatus::Invalid(E2EValidateError::Profile5(
                profile5::ValidateError::TooShort { actual: 2 }
            ))
        );
    }

    #[cfg(feature = "std")]
    #[test]
    fn test_check_result_to_owned_payload() {
        let data = b"hello";
        let result = E2ECheckResult {
            status: E2ECheckStatus::Ok,
            counter: Some(0),
            payload: Some(data),
        };
        assert_eq!(result.to_owned_payload(), Some(b"hello".to_vec()));

        let err_result = E2ECheckResult {
            status: E2ECheckStatus::Invalid(E2EValidateError::Profile5(
                profile5::ValidateError::TooShort { actual: 1 },
            )),
            counter: None,
            payload: None,
        };
        assert_eq!(err_result.to_owned_payload(), None);
    }

    #[test]
    fn test_e2e_key_from_message_id() {
        let mid = crate::protocol::MessageId::new_from_service_and_method(0x1234, 0x0001);
        let key = E2EKey::from_message_id(mid);
        assert_eq!(key.service_id, 0x1234);
        assert_eq!(key.method_or_event_id, 0x0001);
    }
}
