use std::io::{Read, Write};

use super::sd;
use crate::protocol::{Error, Header};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MessagePayload {
    ServiceDiscovery(sd::Header),
    Custom(Vec<u8>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Message {
    header: Header,
    payload: MessagePayload,
}

impl Message {
    pub fn new(header: Header, payload: MessagePayload) -> Self {
        Self { header, payload }
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    pub fn payload(&self) -> &MessagePayload {
        &self.payload
    }

    pub fn payload_mut(&mut self) -> &mut MessagePayload {
        &mut self.payload
    }

    pub fn write<T: Write>(&self, writer: &mut T) -> Result<usize, Error> {
        let header_size = self.header.write(writer)?;
        match &self.payload {
            MessagePayload::ServiceDiscovery(sd_header) => {
                let sd_header_size = sd_header.write(writer)?;
                Ok(header_size + sd_header_size)
            }
            MessagePayload::Custom(payload) => {
                writer.write_all(payload)?;
                Ok(header_size + payload.len())
            }
        }
    }

    pub fn read<T: Read>(message_bytes: &mut T) -> Result<Self, Error> {
        let header = Header::read(message_bytes)?;
        let mut payload = vec![0; (header.length - 8) as usize];
        message_bytes.read_exact(&mut payload)?;
        Ok(Self::new(header, payload))
    }
}
