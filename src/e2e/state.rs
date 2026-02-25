//! State tracking structures for E2E profiles.

/// State for E2E Profile 4 protection/checking.
#[derive(Debug, Clone)]
pub struct Profile4State {
    /// Counter for protection (incremented on each protect call).
    pub(crate) protect_counter: u16,
    /// Last received counter for checking.
    pub(crate) last_counter: Option<u16>,
}

impl Profile4State {
    /// Create a new Profile 4 state with initial counter value of 0.
    #[must_use] 
    pub fn new() -> Self {
        Self {
            protect_counter: 0,
            last_counter: None,
        }
    }

    /// Create a new Profile 4 state with a specific initial counter.
    #[must_use] 
    pub fn with_initial_counter(counter: u16) -> Self {
        Self {
            protect_counter: counter,
            last_counter: None,
        }
    }

    /// Reset the state to initial values.
    pub fn reset(&mut self) {
        self.protect_counter = 0;
        self.last_counter = None;
    }
}

impl Default for Profile4State {
    fn default() -> Self {
        Self::new()
    }
}

/// State for E2E Profile 5 protection/checking.
#[derive(Debug, Clone)]
pub struct Profile5State {
    /// Counter for protection (incremented on each protect call).
    pub(crate) protect_counter: u8,
    /// Last received counter for checking.
    pub(crate) last_counter: Option<u8>,
}

impl Profile5State {
    /// Create a new Profile 5 state with initial counter value of 0.
    #[must_use] 
    pub fn new() -> Self {
        Self {
            protect_counter: 0,
            last_counter: None,
        }
    }

    /// Create a new Profile 5 state with a specific initial counter.
    #[must_use] 
    pub fn with_initial_counter(counter: u8) -> Self {
        Self {
            protect_counter: counter,
            last_counter: None,
        }
    }

    /// Reset the state to initial values.
    pub fn reset(&mut self) {
        self.protect_counter = 0;
        self.last_counter = None;
    }
}

impl Default for Profile5State {
    fn default() -> Self {
        Self::new()
    }
}
