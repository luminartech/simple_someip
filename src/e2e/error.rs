use thiserror::Error;

/// Errors that can occur during E2E protection or checking.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// The output buffer is too small to hold the protected payload.
    #[error("output buffer too small: need {needed} bytes, got {actual}")]
    BufferTooSmall {
        /// The number of bytes required.
        needed: usize,
        /// The number of bytes available.
        actual: usize,
    },
}

impl From<simple_e2e::profile4::ProtectError> for Error {
    fn from(err: simple_e2e::profile4::ProtectError) -> Self {
        match err {
            simple_e2e::profile4::ProtectError::BufferTooSmall { needed, actual } => {
                Self::BufferTooSmall { needed, actual }
            }
        }
    }
}

impl From<simple_e2e::profile5::ProtectError> for Error {
    fn from(err: simple_e2e::profile5::ProtectError) -> Self {
        match err {
            simple_e2e::profile5::ProtectError::BufferTooSmall { needed, actual } => {
                Self::BufferTooSmall { needed, actual }
            }
        }
    }
}
