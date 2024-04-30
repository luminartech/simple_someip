use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("Invalid value in MessageType field: {0:X}")]
    InvalidMessageTypeField(u8),
    #[error("Invalid value in ReturnCode field: {0:X}")]
    InvalidReturnCode(u8),
}
