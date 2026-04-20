#[cfg(feature = "std")]
use thiserror::Error;

/// Errors that can occur during E2E protection or checking.
#[derive(Debug)]
#[cfg_attr(feature = "std", derive(Error))]
pub enum Error {
    /// The output buffer is too small to hold the protected payload.
    #[cfg_attr(
        feature = "std",
        error("output buffer too small: need {needed} bytes, got {actual}")
    )]
    BufferTooSmall {
        /// The number of bytes required.
        needed: usize,
        /// The number of bytes available.
        actual: usize,
    },
}

#[cfg(not(feature = "std"))]
impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BufferTooSmall { needed, actual } => {
                write!(f, "output buffer too small: need {needed} bytes, got {actual}")
            }
        }
    }
}
