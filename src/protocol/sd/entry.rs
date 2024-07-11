use byteorder::{ReadBytesExt, WriteBytesExt};
use std::io::{Read, Write};

use crate::protocol::Error;

use super::{EventGroupEntry, ServiceEntry};

pub const ENTRY_SIZE: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EntryType {
    FindService,
    OfferService,
    StopOfferService,
    Subscribe,
    SubscribeAck,
}

impl TryFrom<u8> for EntryType {
    type Error = Error;
    fn try_from(value: u8) -> Result<Self, Error> {
        match value {
            0x00 => Ok(EntryType::FindService),
            0x01 => Ok(EntryType::OfferService),
            0x02 => Ok(EntryType::StopOfferService),
            0x06 => Ok(EntryType::Subscribe),
            0x07 => Ok(EntryType::SubscribeAck),
            _ => Err(Error::InvalidSDEntryType(value)),
        }
    }
}

impl From<EntryType> for u8 {
    fn from(service_entry_type: EntryType) -> u8 {
        match service_entry_type {
            EntryType::FindService => 0x00,
            EntryType::OfferService => 0x01,
            EntryType::StopOfferService => 0x02,
            EntryType::Subscribe => 0x06,
            EntryType::SubscribeAck => 0x07,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OptionsCount {
    pub first_options_count: u8,
    pub second_options_count: u8,
}

impl From<u8> for OptionsCount {
    fn from(value: u8) -> Self {
        let first_options_count = (value & 0xf0) >> 4;
        let second_options_count = value & 0x0f;

        Self {
            first_options_count,
            second_options_count,
        }
    }
}

impl From<OptionsCount> for u8 {
    fn from(options_count: OptionsCount) -> u8 {
        ((options_count.first_options_count << 4) & 0xf0)
            | (options_count.second_options_count & 0x0f)
    }
}

impl OptionsCount {
    pub fn new(first_options_count: u8, second_options_count: u8) -> Self {
        assert!(first_options_count < 16);
        assert!(second_options_count < 16);
        OptionsCount {
            first_options_count,
            second_options_count,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Entry {
    FindService(ServiceEntry),
    OfferService(ServiceEntry),
    StopOfferService(ServiceEntry),
    SubscribeEventGroup(EventGroupEntry),
    SubscribeAckEventGroup(EventGroupEntry),
}

impl Entry {
    pub fn first_options_count(&self) -> u8 {
        match self {
            Entry::FindService(service_entry) => service_entry.options_count.first_options_count,
            Entry::OfferService(service_entry) => service_entry.options_count.first_options_count,
            Entry::StopOfferService(service_entry) => {
                service_entry.options_count.first_options_count
            }
            Entry::SubscribeEventGroup(event_group_entry) => {
                event_group_entry.options_count.first_options_count
            }
            Entry::SubscribeAckEventGroup(event_group_entry) => {
                event_group_entry.options_count.first_options_count
            }
        }
    }

    pub fn second_options_count(&self) -> u8 {
        match self {
            Entry::FindService(service_entry) => service_entry.options_count.second_options_count,
            Entry::OfferService(service_entry) => service_entry.options_count.second_options_count,
            Entry::StopOfferService(service_entry) => {
                service_entry.options_count.second_options_count
            }
            Entry::SubscribeEventGroup(event_group_entry) => {
                event_group_entry.options_count.second_options_count
            }
            Entry::SubscribeAckEventGroup(event_group_entry) => {
                event_group_entry.options_count.second_options_count
            }
        }
    }

    pub fn total_options_count(&self) -> u8 {
        self.first_options_count() + self.second_options_count()
    }

    pub fn write<T: Write>(&self, writer: &mut T) -> Result<usize, Error> {
        match self {
            Entry::FindService(service_entry) => {
                writer.write_u8(u8::from(EntryType::FindService))?;
                service_entry.write(writer)
            }
            Entry::OfferService(service_entry) => {
                writer.write_u8(u8::from(EntryType::OfferService))?;
                service_entry.write(writer)
            }
            Entry::StopOfferService(service_entry) => {
                writer.write_u8(u8::from(EntryType::StopOfferService))?;
                service_entry.write(writer)
            }
            Entry::SubscribeEventGroup(event_group_entry) => {
                writer.write_u8(u8::from(EntryType::Subscribe))?;
                event_group_entry.write(writer)
            }
            Entry::SubscribeAckEventGroup(event_group_entry) => {
                writer.write_u8(u8::from(EntryType::SubscribeAck))?;
                event_group_entry.write(writer)
            }
        }
    }

    pub fn read<T: Read>(message_bytes: &mut T) -> Result<Self, Error> {
        let entry_type = EntryType::try_from(message_bytes.read_u8()?)?;
        match entry_type {
            EntryType::FindService => {
                let service_entry = ServiceEntry::read(message_bytes)?;
                Ok(Entry::FindService(service_entry))
            }
            EntryType::OfferService => {
                let service_entry = ServiceEntry::read(message_bytes)?;
                Ok(Entry::OfferService(service_entry))
            }
            EntryType::StopOfferService => {
                let service_entry = ServiceEntry::read(message_bytes)?;
                Ok(Entry::StopOfferService(service_entry))
            }
            EntryType::Subscribe => {
                let event_group_entry = EventGroupEntry::read(message_bytes)?;
                Ok(Entry::SubscribeEventGroup(event_group_entry))
            }
            EntryType::SubscribeAck => {
                let event_group_entry = EventGroupEntry::read(message_bytes)?;
                Ok(Entry::SubscribeAckEventGroup(event_group_entry))
            }
        }
    }
}
