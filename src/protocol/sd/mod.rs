mod entry;
mod flags;
mod header;
mod options;

// Export all definitions from the service discovery mod

pub use entry::{
    Entry, EntryIter, EntryType, EntryView, EventGroupEntry, OptionsCount, ServiceEntry,
};
pub use flags::Flags;
pub use header::{Header, MAX_SD_ENTRIES, MAX_SD_OPTIONS, SdEntries, SdHeaderView, SdOptions};
pub use options::{
    MAX_CONFIGURATION_STRING_LENGTH, OptionIter, OptionType, OptionView, Options,
    TransportProtocol, extract_ipv4_endpoint,
};
