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

