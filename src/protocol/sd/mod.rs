mod entry;
mod error;
mod flags;
mod header;
mod options;

#[cfg(test)]
pub(crate) mod test_support;

use core::net::Ipv4Addr;

/// Standard SOME/IP-SD multicast group address (239.255.0.255).
pub const MULTICAST_IP: Ipv4Addr = Ipv4Addr::new(239, 255, 0, 255);
/// Standard SOME/IP-SD port (30490).
pub const MULTICAST_PORT: u16 = 30490;
/// SOME/IP Message ID for service discovery messages (`0xFFFF_8100`).
pub const MESSAGE_ID_VALUE: u32 = 0xffff_8100;

// Export all definitions from the service discovery mod

pub use entry::{
    Entry, EntryIter, EntryType, EntryView, EventGroupEntry, OptionsCount, ServiceEntry,
};
pub use error::Error;
pub use flags::Flags;
pub use header::{Header, SdHeaderView};
pub use options::{
    MAX_CONFIGURATION_STRING_LENGTH, OptionIter, OptionType, OptionView, Options,
    TransportProtocol, extract_ipv4_endpoint,
};
