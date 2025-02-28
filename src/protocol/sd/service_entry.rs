use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use std::io::{Read, Write};

use crate::{protocol::Error, traits::WireFormat};

use super::entry::OptionsCount;

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
    pub fn new_find(service_id: u16) -> Self {
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
    fn from_reader<R: Read>(reader: &mut R) -> Result<Self, Error> {
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

    fn to_writer<W: Write>(&self, writer: &mut W) -> Result<usize, Error> {
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
