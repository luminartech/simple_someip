use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error(transparent)]
    Protocol(#[from] crate::protocol::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("Unexpected discovery message: {0:?}")]
    UnexpectedDiscoveryMessage(crate::protocol::Header),
    #[error("Socket Closed Unexpectedly")]
    SocketClosedUnexpectedly,
    #[error("Unicast Socket not bound")]
    UnicastSocketNotBound,
    #[error("Service not found in endpoint registry")]
    ServiceNotFound,
}
