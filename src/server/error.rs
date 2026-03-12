use thiserror::Error;

/// Errors that can occur during SOME/IP server operations.
#[derive(Error, Debug)]
pub enum Error {
    /// A SOME/IP protocol-level error.
    #[error(transparent)]
    Protocol(#[from] crate::protocol::Error),
    /// An I/O error from the underlying network transport.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// An E2E protection or checking error occurred.
    #[error(transparent)]
    E2e(#[from] crate::e2e::Error),
}

impl From<crate::protocol::sd::Error> for Error {
    fn from(err: crate::protocol::sd::Error) -> Self {
        Self::Protocol(crate::protocol::Error::from(err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_sd_error_produces_protocol_variant() {
        let sd_err = crate::protocol::sd::Error::IncorrectEntriesSize(0);
        let err: Error = sd_err.into();
        assert!(matches!(err, Error::Protocol(_)));
    }
}
