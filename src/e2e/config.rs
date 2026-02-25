//! Configuration structures for E2E profiles.

/// Configuration for E2E Profile 4.
#[derive(Debug, Clone)]
pub struct Profile4Config {
    /// Unique identifier for this data element (included in CRC calculation).
    pub data_id: u32,
    /// Maximum allowed counter delta before reporting `WrongSequence`.
    /// A delta of 1 means consecutive messages, delta > 1 means some lost.
    pub max_delta_counter: u16,
}

impl Profile4Config {
    /// Create a new Profile 4 configuration.
    ///
    /// # Arguments
    /// * `data_id` - Unique identifier for this data element
    /// * `max_delta_counter` - Maximum allowed gap in counter sequence
    #[must_use] 
    pub fn new(data_id: u32, max_delta_counter: u16) -> Self {
        Self {
            data_id,
            max_delta_counter,
        }
    }
}

/// Configuration for E2E Profile 5.
#[derive(Debug, Clone)]
pub struct Profile5Config {
    /// Unique identifier for this data element (included in CRC calculation).
    /// Profile 5 uses a 16-bit `DataID`.
    pub data_id: u16,
    /// Expected data length (used in CRC calculation).
    pub data_length: u16,
    /// Maximum allowed counter delta before reporting `WrongSequence`.
    pub max_delta_counter: u8,
}

impl Profile5Config {
    /// Create a new Profile 5 configuration.
    ///
    /// # Arguments
    /// * `data_id` - Unique identifier for this data element
    /// * `data_length` - Expected length of protected data
    /// * `max_delta_counter` - Maximum allowed gap in counter sequence
    #[must_use] 
    pub fn new(data_id: u16, data_length: u16, max_delta_counter: u8) -> Self {
        Self {
            data_id,
            data_length,
            max_delta_counter,
        }
    }
}
