use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error(transparent)]
    ProtocolError(#[from] crate::protocol::Error),
}
