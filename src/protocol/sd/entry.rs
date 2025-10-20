use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use std::io::{Read, Write};

use crate::{protocol::Error, traits::WireFormat};

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
pub struct EventGroupEntry {
    pub index_first_options_run: u8,
    pub index_second_options_run: u8,
    pub options_count: OptionsCount,
    pub service_id: u16,
    pub instance_id: u16,
    pub major_version: u8,
    /// ttl is a u24 value
    pub ttl: u32,
    pub counter: u16,
    pub event_group_id: u16,
}

impl EventGroupEntry {
    pub fn new(
        service_id: u16,
        instance_id: u16,
        major_version: u8,
        ttl: u32,
        event_group_id: u16,
    ) -> Self {
        Self {
            index_first_options_run: 0,
            index_second_options_run: 0,
            options_count: OptionsCount::new(1, 0),
            service_id,
            instance_id,
            major_version,
            ttl,
            counter: 0,
            event_group_id,
        }
    }
}

impl WireFormat for EventGroupEntry {
    fn decode<T: std::io::Read>(reader: &mut T) -> Result<Self, crate::protocol::Error> {
        let index_first_options_run = reader.read_u8()?;
        let index_second_options_run = reader.read_u8()?;
        let options_count = OptionsCount::from(reader.read_u8()?);
        let service_id = reader.read_u16::<BigEndian>()?;
        let instance_id = reader.read_u16::<BigEndian>()?;
        let major_version = reader.read_u8()?;
        let ttl = reader.read_u24::<BigEndian>()?;
        let counter = reader.read_u16::<BigEndian>()? & 0x000f;
        let event_group_id = reader.read_u16::<BigEndian>()?;
        Ok(Self {
            index_first_options_run,
            index_second_options_run,
            options_count,
            service_id,
            instance_id,
            major_version,
            ttl,
            counter,
            event_group_id,
        })
    }

    fn required_size(&self) -> usize {
        16
    }

    fn encode<T: std::io::Write>(
        &self,
        writer: &mut T,
    ) -> Result<usize, crate::protocol::Error> {
        writer.write_u8(self.index_first_options_run)?;
        writer.write_u8(self.index_second_options_run)?;
        writer.write_u8(u8::from(self.options_count))?;
        writer.write_u16::<BigEndian>(self.service_id)?;
        writer.write_u16::<BigEndian>(self.instance_id)?;
        writer.write_u8(self.major_version)?;
        writer.write_u24::<BigEndian>(self.ttl)?;
        writer.write_u16::<BigEndian>(self.counter)?;
        writer.write_u16::<BigEndian>(self.event_group_id)?;
        Ok(16)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceEntry {
    pub index_first_options_run: u8,
    pub index_second_options_run: u8,
    pub options_count: OptionsCount,
    pub service_id: u16,
    pub instance_id: u16,
    pub major_version: u8,
    /// ttl is a u24 value
    pub ttl: u32,
    pub minor_version: u32,
}

impl ServiceEntry {
    pub fn find(service_id: u16) -> Self {
        Self {
            index_first_options_run: 0,
            index_second_options_run: 0,
            options_count: OptionsCount::new(1, 0),
            service_id,
            instance_id: 0xFFFF,
            major_version: 0xFF,
            ttl: 0x00FFFFFF,
            minor_version: 0xFFFFFFFF,
        }
    }
}

impl WireFormat for ServiceEntry {
    fn decode<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let index_first_options_run = reader.read_u8()?;
        let index_second_options_run = reader.read_u8()?;
        let options_count = OptionsCount::from(reader.read_u8()?);
        let service_id = reader.read_u16::<BigEndian>()?;
        let instance_id = reader.read_u16::<BigEndian>()?;
        let major_version = reader.read_u8()?;
        let ttl = reader.read_u24::<BigEndian>()?;
        let minor_version = reader.read_u32::<BigEndian>()?;
        Ok(Self {
            index_first_options_run,
            index_second_options_run,
            options_count,
            service_id,
            instance_id,
            major_version,
            ttl,
            minor_version,
        })
    }

    fn required_size(&self) -> usize {
        16
    }

    fn encode<W: Write>(&self, writer: &mut W) -> Result<usize, Error> {
        writer.write_u8(self.index_first_options_run)?;
        writer.write_u8(self.index_second_options_run)?;
        writer.write_u8(u8::from(self.options_count))?;
        writer.write_u16::<BigEndian>(self.service_id)?;
        writer.write_u16::<BigEndian>(self.instance_id)?;
        writer.write_u8(self.major_version)?;
        writer.write_u24::<BigEndian>(self.ttl)?;
        writer.write_u32::<BigEndian>(self.minor_version)?;
        Ok(16)
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
}

impl WireFormat for Entry {
    fn decode<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let entry_type = EntryType::try_from(reader.read_u8()?)?;
        match entry_type {
            EntryType::FindService => {
                let service_entry = ServiceEntry::decode(reader)?;
                Ok(Entry::FindService(service_entry))
            }
            EntryType::OfferService => {
                let service_entry = ServiceEntry::decode(reader)?;
                Ok(Entry::OfferService(service_entry))
            }
            EntryType::StopOfferService => {
                let service_entry = ServiceEntry::decode(reader)?;
                Ok(Entry::StopOfferService(service_entry))
            }
            EntryType::Subscribe => {
                let event_group_entry = EventGroupEntry::decode(reader)?;
                Ok(Entry::SubscribeEventGroup(event_group_entry))
            }
            EntryType::SubscribeAck => {
                let event_group_entry = EventGroupEntry::decode(reader)?;
                Ok(Entry::SubscribeAckEventGroup(event_group_entry))
            }
        }
    }

    fn required_size(&self) -> usize {
        1 + match self {
            Entry::FindService(service_entry) => service_entry.required_size(),
            Entry::OfferService(service_entry) => service_entry.required_size(),
            Entry::StopOfferService(service_entry) => service_entry.required_size(),
            Entry::SubscribeEventGroup(event_group_entry) => event_group_entry.required_size(),
            Entry::SubscribeAckEventGroup(event_group_entry) => event_group_entry.required_size(),
        }
    }

    fn encode<W: Write>(&self, writer: &mut W) -> Result<usize, Error> {
        match self {
            Entry::FindService(service_entry) => {
                writer.write_u8(u8::from(EntryType::FindService))?;
                service_entry.encode(writer)
            }
            Entry::OfferService(service_entry) => {
                writer.write_u8(u8::from(EntryType::OfferService))?;
                service_entry.encode(writer)
            }
            Entry::StopOfferService(service_entry) => {
                writer.write_u8(u8::from(EntryType::StopOfferService))?;
                service_entry.encode(writer)
            }
            Entry::SubscribeEventGroup(event_group_entry) => {
                writer.write_u8(u8::from(EntryType::Subscribe))?;
                event_group_entry.encode(writer)
            }
            Entry::SubscribeAckEventGroup(event_group_entry) => {
                writer.write_u8(u8::from(EntryType::SubscribeAck))?;
                event_group_entry.encode(writer)
            }
        }
    }
}
