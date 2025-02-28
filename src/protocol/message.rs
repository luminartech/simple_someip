use std::io::{Read, Write};

use crate::{
    protocol::{Error, Header, MessageType, ReturnCode, sd},
    traits::{PayloadWireFormat, WireFormat},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Message<PayloadDefinition> {
    header: Header,
    payload: PayloadDefinition,
}

impl<PayloadDefinition: PayloadWireFormat> Message<PayloadDefinition> {
    pub fn new(header: Header, payload: PayloadDefinition) -> Self {
        Self { header, payload }
    }

    pub fn new_sd(session_id: u32, sd_header: &sd::Header) -> Self {
        let sd_header_size = sd_header.size();
        Self::new(
            Header::new_sd(session_id, sd_header_size),
            PayloadDefinition::new_sd_payload(&sd_header),
        )
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    pub const fn is_sd(&self) -> bool {
        self.header.is_sd()
    }

    pub fn payload(&self) -> &PayloadDefinition {
        &self.payload
    }

    pub fn payload_mut(&mut self) -> &mut PayloadDefinition {
        &mut self.payload
    }
}

impl<PayloadDefinition: PayloadWireFormat> WireFormat for Message<PayloadDefinition> {
    fn from_reader<R: Read>(reader: &mut R) -> Result<Self, Error> {
        let header = Header::from_reader(reader)?;
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
        }
        let mut payload_reader = reader.take(header.payload_size() as u64);
        let payload =
            PayloadDefinition::from_reader_with_message_id(header.message_id, &mut payload_reader)?;
        Ok(Self::new(header, payload))
    }

    fn required_size(&self) -> usize {
        self.header.required_size() + self.payload.required_size()
    }

    fn to_writer<W: Write>(&self, writer: &mut W) -> Result<usize, Error> {
        Ok(self.header.to_writer(writer)? + self.payload.to_writer(writer)?)
    }
}
