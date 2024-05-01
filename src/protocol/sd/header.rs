use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use std::io::{Read, Write};

use crate::protocol::Error;

use super::{entry::ENTRY_SIZE, Entry, Flags, Options};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Header {
    pub flags: Flags,
    pub entries: Vec<Entry>,
    pub options: Vec<Options>,
}

impl Header {
    pub fn write<T: Write>(&self, writer: &mut T) -> Result<usize, Error> {
        writer.write_u8(u8::from(self.flags))?;
        let reserved: [u8; 3] = [0; 3];
        writer.write_all(&reserved)?;
        let entries_size = (self.entries.len() * 4) as u32;
        writer.write_u32::<BigEndian>(entries_size)?;
        for entry in &self.entries {
            entry.write(writer)?;
        }
        let options_size = (self.options.len() * 4) as u32;
        writer.write_u32::<BigEndian>(options_size)?;
        for option in &self.options {
            option.write(writer)?;
        }
        Ok(12 + entries_size as usize + options_size as usize)
    }

    pub fn read<T: Read>(message_bytes: &mut T) -> Result<Self, Error> {
        let flags = Flags::from(message_bytes.read_u8()?);
        let mut reserved: [u8; 3] = [0; 3];
        message_bytes.read_exact(&mut reserved)?;
        let entries_size = message_bytes.read_u32::<BigEndian>()?;
        let entries_count = entries_size / ENTRY_SIZE as u32;
        let mut entries = Vec::with_capacity(entries_count as usize);
        let mut options_count = 0;
        for i in 0..entries_count {
            entries.push(Entry::read(message_bytes)?);
            match &entries[i as usize] {
                Entry::Service(_, service_entry) => {
                    options_count += service_entry.options_count.first_options_count as u32;
                    options_count += service_entry.options_count.second_options_count as u32;
                }
                Entry::EventGroup(..) => (),
            }
        }

        let mut remaining_options_size = message_bytes.read_u32::<BigEndian>()?;
        let mut options = Vec::with_capacity(options_count as usize);
        for i in 0..options_count {
            options.push(Options::read(message_bytes)?);
            remaining_options_size -= options[i as usize].size();
        }
        if remaining_options_size != 0 {
            return Err(Error::IncorrectOptionsSize(remaining_options_size));
        }
        Ok(Self {
            flags,
            entries,
            options,
        })
    }
}
