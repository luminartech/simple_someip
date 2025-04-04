use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error(transparent)]
    ProtocolError(#[from] crate::protocol::Error),
    #[error(transparent)]
    Io(#[from] tokio::io::Error),
    #[error("Unexpected discovery message: {0:?}")]
    UnexpectedDiscoveryMessage(crate::protocol::Header),
    #[error("Invalid SD Header: {0:?}")]
    InvalidSDHeader(crate::protocol::sd::Header),
    #[error("Socket Closed Unexpectedly")]
    SocketClosedUnexpectedly,
    #[error("Unicast Socket not bound")]
    UnicastSocketNotBound,
}
