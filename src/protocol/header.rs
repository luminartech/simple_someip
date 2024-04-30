use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use std::io::{Read, Write};

use super::{Error, MessageId, MessageTypeField, ReturnCode};

/// SOME/IP header
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Header {
    /// Message ID, encoding service ID and method ID
    pub message_id: MessageId,
    /// Length of the message in bytes, starting at the request Id
    /// Total length of the message is therefore length + 8
    pub length: u32,
    pub request_id: u32,
    pub protocol_version: u8,
    pub interface_version: u8,
    pub message_type: MessageTypeField,
    pub return_code: ReturnCode,
}

impl Header {
    pub fn read<T: Read>(message_bytes: &mut T) -> Result<Self, Error> {
        let message_id = MessageId::from(message_bytes.read_u32::<BigEndian>()?);
        let length = message_bytes.read_u32::<BigEndian>()?;
        let request_id = message_bytes.read_u32::<BigEndian>()?;
        let protocol_version = message_bytes.read_u8()?;
        let interface_version = message_bytes.read_u8()?;
        let message_type = MessageTypeField::try_from(message_bytes.read_u8()?)?;
        let return_code = ReturnCode::try_from(message_bytes.read_u8()?)?;
        Ok(Self {
            message_id,
            length,
            request_id,
            protocol_version,
            interface_version,
            message_type,
            return_code,
        })
    }

    pub fn write<T: Write>(&self, writer: &mut T) -> Result<usize, Error> {
        writer.write_u32::<BigEndian>(self.message_id.message_id())?;
        writer.write_u32::<BigEndian>(self.length)?;
        writer.write_u32::<BigEndian>(self.request_id)?;
        writer.write_u8(self.protocol_version)?;
        writer.write_u8(self.interface_version)?;
        writer.write_u8(u8::from(self.message_type))?;
        writer.write_u8(u8::from(self.return_code))?;
        Ok(16)
    }

    pub fn payload_size(&self) -> usize {
        self.length as usize - 8
    }
}
