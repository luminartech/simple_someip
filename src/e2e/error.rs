use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("output buffer too small: need {needed} bytes, got {actual}")]
    BufferTooSmall { needed: usize, actual: usize },
}
