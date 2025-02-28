use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};

use crate::traits::WireFormat;

use super::entry::OptionsCount;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventGroupEntry {
    index_first_options_run: u8,
    index_second_options_run: u8,
    pub(crate) options_count: OptionsCount,
    service_id: u16,
    instance_id: u16,
    major_version: u8,
    /// ttl is a u24 value
    ttl: u32,
    counter: u16,
    event_group_id: u16,
}

impl EventGroupEntry {
    pub fn new_subscription(
        service_id: u16,
        instance_id: u16,
        major_version: u8,
        ttl: u32,
        counter: u8,
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
            counter: (counter & 0x000f) as u16,
            event_group_id,
        }
    }
}

impl WireFormat for EventGroupEntry {
    fn from_reader<T: std::io::Read>(reader: &mut T) -> Result<Self, crate::protocol::Error> {
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

    fn to_writer<T: std::io::Write>(
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
