use super::Error;

///Return code contained in a SOME/IP header.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum ReturnCode {
    Ok,
    NotOk,
    UnknownService,
    UnknownMethod,
    NotReady,
    NotReachable,
    Timeout,
    WrongProtocolVersion,
    WrongInterfaceVersion,
    MalformedMessage,
    WrongMessageType,
    E2ERepeated,
    E2EWrongSequence,
    E2E,
    E2ENotAvailable,
    E2ENoNewData,
    GenericError(u8),
    InterfaceError(u8),
}

impl TryFrom<u8> for ReturnCode {
    type Error = Error;
    fn try_from(value: u8) -> Result<Self, Error> {
        match value {
            0x00 => Ok(ReturnCode::Ok),
            0x01 => Ok(ReturnCode::NotOk),
            0x02 => Ok(ReturnCode::UnknownService),
            0x03 => Ok(ReturnCode::UnknownMethod),
            0x04 => Ok(ReturnCode::NotReady),
            0x05 => Ok(ReturnCode::NotReachable),
            0x06 => Ok(ReturnCode::Timeout),
            0x07 => Ok(ReturnCode::WrongProtocolVersion),
            0x08 => Ok(ReturnCode::WrongInterfaceVersion),
            0x09 => Ok(ReturnCode::MalformedMessage),
            0x0a => Ok(ReturnCode::WrongMessageType),
            0x0b => Ok(ReturnCode::E2ERepeated),
            0x0c => Ok(ReturnCode::E2EWrongSequence),
            0x0d => Ok(ReturnCode::E2E),
            0x0e => Ok(ReturnCode::E2ENotAvailable),
            0x0f => Ok(ReturnCode::E2ENoNewData),
            0x10..=0x1f => Ok(ReturnCode::GenericError(value)),
            0x20..=0x5e => Ok(ReturnCode::InterfaceError(value)),
            _ => Err(Error::InvalidReturnCode(value)),
        }
    }
}

impl From<ReturnCode> for u8 {
    fn from(return_code: ReturnCode) -> u8 {
        match return_code {
            ReturnCode::Ok => 0x00,
            ReturnCode::NotOk => 0x01,
            ReturnCode::UnknownService => 0x02,
            ReturnCode::UnknownMethod => 0x03,
            ReturnCode::NotReady => 0x04,
            ReturnCode::NotReachable => 0x05,
            ReturnCode::Timeout => 0x06,
            ReturnCode::WrongProtocolVersion => 0x07,
            ReturnCode::WrongInterfaceVersion => 0x08,
            ReturnCode::MalformedMessage => 0x09,
            ReturnCode::WrongMessageType => 0x0a,
            ReturnCode::E2ERepeated => 0x0b,
            ReturnCode::E2EWrongSequence => 0x0c,
            ReturnCode::E2E => 0x0d,
            ReturnCode::E2ENotAvailable => 0x0e,
            ReturnCode::E2ENoNewData => 0x0f,
            ReturnCode::GenericError(value) | ReturnCode::InterfaceError(value) => value,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every valid u8 should round-trip: TryFrom then From gives back the same byte.
    // Invalid bytes (0x5f..=0xff) must return an error.
    #[test]
    fn all_valid_u8_values_round_trip() {
        for byte in 0x00u8..=0xffu8 {
            match ReturnCode::try_from(byte) {
                Ok(code) => {
                    assert!(
                        byte < 0x5f,
                        "0x{byte:02X} should be invalid but decoded successfully"
                    );
                    assert_eq!(u8::from(code), byte, "round-trip failed for 0x{byte:02X}");
                }
                Err(_) => {
                    assert!(
                        byte >= 0x5f,
                        "0x{byte:02X} should be valid but failed to decode"
                    );
                }
            }
        }
    }

    // Named variants
    #[test]
    fn named_variants_decode_correctly() {
        let cases = [
            (0x00, ReturnCode::Ok),
            (0x01, ReturnCode::NotOk),
            (0x02, ReturnCode::UnknownService),
            (0x03, ReturnCode::UnknownMethod),
            (0x04, ReturnCode::NotReady),
            (0x05, ReturnCode::NotReachable),
            (0x06, ReturnCode::Timeout),
            (0x07, ReturnCode::WrongProtocolVersion),
            (0x08, ReturnCode::WrongInterfaceVersion),
            (0x09, ReturnCode::MalformedMessage),
            (0x0a, ReturnCode::WrongMessageType),
            (0x0b, ReturnCode::E2ERepeated),
            (0x0c, ReturnCode::E2EWrongSequence),
            (0x0d, ReturnCode::E2E),
            (0x0e, ReturnCode::E2ENotAvailable),
            (0x0f, ReturnCode::E2ENoNewData),
        ];
        for (byte, expected) in cases {
            assert_eq!(ReturnCode::try_from(byte).unwrap(), expected);
        }
    }

    // GenericError range: 0x10..=0x1f
    #[test]
    fn generic_error_range_decodes_and_preserves_value() {
        for byte in 0x10u8..=0x1f {
            assert_eq!(
                ReturnCode::try_from(byte).unwrap(),
                ReturnCode::GenericError(byte)
            );
        }
    }

    // InterfaceError range: 0x20..=0x5e
    #[test]
    fn interface_error_range_decodes_and_preserves_value() {
        for byte in 0x20u8..=0x5e {
            assert_eq!(
                ReturnCode::try_from(byte).unwrap(),
                ReturnCode::InterfaceError(byte)
            );
        }
    }

    // Invalid values
    #[test]
    fn invalid_values_return_error() {
        for byte in [0x5f, 0x60, 0xff] {
            assert!(
                ReturnCode::try_from(byte).is_err(),
                "0x{byte:02X} should be invalid"
            );
        }
    }
}
