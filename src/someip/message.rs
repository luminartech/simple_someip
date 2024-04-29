use std::io::{Read, Write};

use crate::{Error, Header};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Message {
    header: Header,
    payload: Vec<u8>,
}

impl Message {
    pub fn new(header: Header, payload: Vec<u8>) -> Self {
        Self { header, payload }
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    pub fn payload_mut(&mut self) -> &mut Vec<u8> {
        &mut self.payload
    }

    pub fn write<T: Write>(&self, writer: &mut T) -> Result<usize, Error> {
        let header_size = self.header.write(writer)?;
        writer.write_all(&self.payload)?;
        Ok(header_size + self.payload.len())
    }

    pub fn read<T: Read>(message_bytes: &mut T) -> Result<Self, Error> {
        let header = Header::read(message_bytes)?;
        let mut payload = vec![0; (header.length - 8) as usize];
        message_bytes.read_exact(&mut payload)?;
        Ok(Self::new(header, payload))
    }
}
