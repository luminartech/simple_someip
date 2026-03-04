use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error(transparent)]
    Protocol(#[from] crate::protocol::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl From<crate::protocol::sd::Error> for Error {
    fn from(err: crate::protocol::sd::Error) -> Self {
        Self::Protocol(crate::protocol::Error::from(err))
    }
}
