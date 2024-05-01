use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("Invalid protocol version: {0:X}")]
    InvalidProtocolVersion(u8),
    #[error("Invalid value in MessageType field: {0:X}")]
    InvalidMessageTypeField(u8),
    #[error("Invalid value in ReturnCode field: {0:X}")]
    InvalidReturnCode(u8),
    #[error("Invalid value for Service Discovery entry type: {0:X}")]
    InvalidSDEntryType(u8),
    #[error("Invalid value for Service Discovery Option Type: {0:X}")]
    InvalidSDOptionType(u8),
    #[error("Invalid value for Service Discovery Option Transport Protocol: {0:X}")]
    InvalidSDOptionTransportProtocol(u8),
}
