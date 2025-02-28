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
            ReturnCode::GenericError(value) => value,
            ReturnCode::InterfaceError(value) => value,
        }
    }
}
