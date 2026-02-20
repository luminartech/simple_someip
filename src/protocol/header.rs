use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};

use crate::{
    protocol::{Error, MessageId, MessageTypeField, ReturnCode},
    traits::WireFormat,
};

/// SOME/IP header
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Header {
    /// Message ID, encoding service ID and method ID
    pub message_id: MessageId,
    /// Length of the message in bytes, starting at the request Id
    /// Total length of the message is therefore length + 8
    pub length: u32,
    pub session_id: u32,
    pub protocol_version: u8,
    pub interface_version: u8,
    pub message_type: MessageTypeField,
    pub return_code: ReturnCode,
}

impl Header {
    #[must_use] 
    pub fn new_sd(session_id: u32, sd_header_size: usize) -> Self {
        Self {
            message_id: MessageId::SD,
            length: 8 + u32::try_from(sd_header_size).expect("SD header too large"),
            session_id,
            protocol_version: 0x01,
            interface_version: 0x01,
            message_type: MessageTypeField::new_sd(),
            return_code: ReturnCode::Ok,
        }
    }

    #[must_use] 
    pub const fn is_sd(&self) -> bool {
        self.message_id.is_sd()
    }

    #[must_use] 
    pub const fn payload_size(&self) -> usize {
        self.length as usize - 8
    }

    pub fn set_session_id(&mut self, session_id: u32) {
        self.session_id = session_id;
    }
}

impl WireFormat for Header {
    fn decode<T: std::io::Read>(reader: &mut T) -> Result<Self, Error> {
        let message_id = MessageId::from(reader.read_u32::<BigEndian>()?);
        let length = reader.read_u32::<BigEndian>()?;
        let request_id = reader.read_u32::<BigEndian>()?;
        let protocol_version = reader.read_u8()?;
        if protocol_version != 0x01 {
            return Err(Error::InvalidProtocolVersion(protocol_version));
        }
        let interface_version = reader.read_u8()?;
        let message_type = MessageTypeField::try_from(reader.read_u8()?)?;
        let return_code = ReturnCode::try_from(reader.read_u8()?)?;
        Ok(Self {
            message_id,
            length,
            session_id: request_id,
            protocol_version,
            interface_version,
            message_type,
            return_code,
        })
    }

    fn required_size(&self) -> usize {
        16
    }

    fn encode<T: std::io::Write>(&self, writer: &mut T) -> Result<usize, Error> {
        writer.write_u32::<BigEndian>(self.message_id.message_id())?;
        writer.write_u32::<BigEndian>(self.length)?;
        writer.write_u32::<BigEndian>(self.session_id)?;
        writer.write_u8(self.protocol_version)?;
        writer.write_u8(self.interface_version)?;
        writer.write_u8(u8::from(self.message_type))?;
        writer.write_u8(u8::from(self.return_code))?;
        Ok(16)
    }
}
