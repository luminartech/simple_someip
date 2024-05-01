use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use std::io::{Read, Write};

use crate::protocol::Error;

use super::entry::OptionsCount;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventGroupEntry {
    index_first_options_run: u8,
    index_second_options_run: u8,
    options_count: OptionsCount,
    service_id: u16,
    instance_id: u16,
    major_version: u8,
    /// ttl is a u24 value
    ttl: u32,
    minor_version: u32,
}

impl EventGroupEntry {
    pub fn write<T: Write>(&self, writer: &mut T) -> Result<usize, Error> {
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

    pub fn read<T: Read>(message_bytes: &mut T) -> Result<Self, Error> {
        let index_first_options_run = message_bytes.read_u8()?;
        let index_second_options_run = message_bytes.read_u8()?;
        let options_count = OptionsCount::from(message_bytes.read_u8()?);
        let service_id = message_bytes.read_u16::<BigEndian>()?;
        let instance_id = message_bytes.read_u16::<BigEndian>()?;
        let major_version = message_bytes.read_u8()?;
        let ttl = message_bytes.read_u24::<BigEndian>()?;
        let minor_version = message_bytes.read_u32::<BigEndian>()?;
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
}
