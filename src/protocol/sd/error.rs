use thiserror::Error;

/// Errors that can occur when parsing or validating SOME/IP-SD messages.
#[derive(Error, Debug)]
pub enum Error {
    /// The entry type byte is not a recognized SD entry type.
    #[error("Invalid value for Service Discovery entry type: {0:X}")]
    InvalidEntryType(u8),
    /// The option type byte is not a recognized SD option type.
    #[error("Invalid value for Service Discovery Option Type: {0:X}")]
    InvalidOptionType(u8),
    /// The transport protocol byte is not a recognized value.
    #[error("Invalid value for Service Discovery Option Transport Protocol: {0:X}")]
    InvalidOptionTransportProtocol(u8),
    /// The declared options size does not match the actual data.
    #[error("Incorrect options size, {0} bytes remaining")]
    IncorrectOptionsSize(usize),
    /// An option's length field does not match the expected size for its type.
    #[error(
        "Invalid SD option length for type 0x{option_type:02X}: expected {expected}, got {actual}"
    )]
    InvalidOptionLength {
        /// The option type byte.
        option_type: u8,
        /// The expected length value.
        expected: u16,
        /// The actual length value found.
        actual: u16,
    },
    /// A configuration string exceeds the maximum allowed length.
    #[error("Configuration string too long: {0} bytes")]
    ConfigurationStringTooLong(usize),
    /// An SD message failed structural validation.
    #[error("Invalid SD message: {0}")]
    InvalidMessage(&'static str),
    /// The entries array length is not a multiple of the entry size (16 bytes).
    #[error("Entries array length {0} is not a multiple of entry size (16)")]
    IncorrectEntriesSize(usize),
}
