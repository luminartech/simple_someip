use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error(transparent)]
    ProtocolError(#[from] crate::protocol::Error),
    #[cfg(feature = "std")]
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[cfg(feature = "std")]
    #[error("Unexpected discovery message: {0:?}")]
    UnexpectedDiscoveryMessage(crate::protocol::Header),
    #[cfg(feature = "std")]
    #[error("Socket Closed Unexpectedly")]
    SocketClosedUnexpectedly,
    #[cfg(feature = "std")]
    #[error("Unicast Socket not bound")]
    UnicastSocketNotBound,
    #[cfg(feature = "std")]
    #[error("Service not found in endpoint registry")]
    ServiceNotFound,
    #[error("output buffer too small: need {needed} bytes, got {actual}")]
    BufferTooSmall { needed: usize, actual: usize },
}
