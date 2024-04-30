use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use std::io::{Read, Write};

use crate::protocol::Error;

use super::{options, Flags, Options, ServiceEntry};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Header {
    flags: Flags,
    entries: Vec<ServiceEntry>,
    options: Vec<Options>,
}

impl Header {
    pub fn write<T: Write>(&self, writer: &mut T) -> Result<usize, Error> {
        writer.write_u8(u8::from(self.flags))?;
        let reserved: [u8; 3] = [0; 3];
        writer.write_all(&reserved)?;
        let entries_size = (self.entries.len() * 4) as u32;
        writer.write_u32(entries_size)?;
        for entry in &self.entries {
            entry.write(writer)?;
        }
        let options_size = (self.options.len() * 4) as u32;
        Ok(12 + entries_size as usize + options_size as usize)
    }

    pub fn read<T: Read>(message_bytes: &mut T) -> Result<Self, Error> {
        let header = Header::read(message_bytes)?;
        let mut payload = vec![0; (header.length - 8) as usize];
        message_bytes.read_exact(&mut payload)?;
        Ok(Self::new(header, payload))
    }
}
