use std::io::{Read, Write};

use super::sd;
use crate::protocol::{Error, Header, MessageType, ReturnCode};

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

    pub fn new_sd(session_id: u32, sd_header: sd::Header) -> Self {
        let sd_header_size = sd_header.size();
        Self::new(
            Header::new_sd(session_id, sd_header_size),
            MessagePayload::ServiceDiscovery(sd_header),
        )
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
        if header.message_id.is_sd() {
            assert!(header.payload_size() >= 12, "SD message too short");
            assert!(
                header.protocol_version == 0x01,
                "SD protocol version mismatch"
            );
            assert!(
                header.interface_version == 0x01,
                "SD interface version mismatch"
            );
            assert!(
                header.message_type.message_type() == MessageType::Notification,
                "SD message type mismatch"
            );
            assert!(
                header.return_code == ReturnCode::Ok,
                "SD return code mismatch"
            );
            let sd_header = sd::Header::read(message_bytes)?;

            Ok(Self::new(
                header,
                MessagePayload::ServiceDiscovery(sd_header),
            ))
        } else {
            let mut payload = vec![0; header.payload_size()];
            message_bytes.read_exact(&mut payload)?;
            Ok(Self::new(header, MessagePayload::Custom(payload)))
        }
    }
}
