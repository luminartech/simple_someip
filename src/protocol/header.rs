use crate::{
    protocol::{
        Error, MessageId, MessageTypeField, ReturnCode,
        byte_order::{ReadBytesExt, WriteBytesExt},
    },
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
    /// SOME/IP Request ID (4 bytes): Client ID [31:16] + Session ID [15:0].
    pub request_id: u32,
    pub protocol_version: u8,
    pub interface_version: u8,
    pub message_type: MessageTypeField,
    pub return_code: ReturnCode,
}

impl Header {
    /// Return the 8-byte "upper header" used by E2E UPPER-HEADER-BITS-TO-SHIFT.
    ///
    /// Layout (big-endian): `request_id(4)` + `protocol_version(1)` + `interface_version(1)`
    ///                      + `message_type(1)` + `return_code(1)`
    ///
    /// Note: `request_id` is the full 4-byte SOME/IP Request ID field
    /// (Client ID [31:16] + Session ID [15:0]), not just the 2-byte Session ID.
    #[must_use]
    pub fn upper_header_bytes(&self) -> [u8; 8] {
        let rid = self.request_id.to_be_bytes();
        [
            rid[0],
            rid[1],
            rid[2],
            rid[3],
            self.protocol_version,
            self.interface_version,
            u8::from(self.message_type),
            u8::from(self.return_code),
        ]
    }

    #[must_use]
    pub fn new_sd(request_id: u32, sd_header_size: usize) -> Self {
        Self {
            message_id: MessageId::SD,
            length: 8 + u32::try_from(sd_header_size).expect("SD header too large"),
            request_id,
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

    pub fn set_request_id(&mut self, request_id: u32) {
        self.request_id = request_id;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Error, MessageId, MessageTypeField, ReturnCode};

    fn make_header() -> Header {
        Header {
            message_id: MessageId::new_from_service_and_method(0x1234, 0x0001),
            length: 16,
            request_id: 0xABCD_0042,
            protocol_version: 0x01,
            interface_version: 0x03,
            message_type: MessageTypeField::try_from(0x00).unwrap(), // Request
            return_code: ReturnCode::Ok,
        }
    }

    fn encode_header(h: &Header) -> [u8; 16] {
        let mut buf = [0u8; 16];
        h.encode(&mut buf.as_mut_slice()).unwrap();
        buf
    }

    // --- upper_header_bytes ---

    #[test]
    fn upper_header_bytes_layout() {
        let h = make_header();
        let ub = h.upper_header_bytes();
        let rid = h.request_id.to_be_bytes();
        assert_eq!(ub[0..4], rid);
        assert_eq!(ub[4], h.protocol_version);
        assert_eq!(ub[5], h.interface_version);
        assert_eq!(ub[6], u8::from(h.message_type));
        assert_eq!(ub[7], u8::from(h.return_code));
    }

    // --- new_sd ---

    #[test]
    fn new_sd_fields() {
        let h = Header::new_sd(0x0000_0001, 28);
        assert_eq!(h.message_id, MessageId::SD);
        assert_eq!(h.length, 8 + 28);
        assert_eq!(h.request_id, 0x0000_0001);
        assert_eq!(h.protocol_version, 0x01);
        assert_eq!(h.interface_version, 0x01);
        assert_eq!(h.return_code, ReturnCode::Ok);
    }

    // --- is_sd ---

    #[test]
    fn is_sd_true_for_sd_header() {
        let h = Header::new_sd(0, 12);
        assert!(h.is_sd());
    }

    #[test]
    fn is_sd_false_for_non_sd_header() {
        let h = make_header();
        assert!(!h.is_sd());
    }

    // --- payload_size ---

    #[test]
    fn payload_size_returns_length_minus_8() {
        let h = Header {
            length: 24,
            ..make_header()
        };
        assert_eq!(h.payload_size(), 16);
    }

    // --- set_request_id ---

    #[test]
    fn set_request_id_updates_value() {
        let mut h = make_header();
        h.set_request_id(0xDEAD_BEEF);
        assert_eq!(h.request_id, 0xDEAD_BEEF);
    }

    // --- required_size ---

    #[test]
    fn required_size_is_16() {
        assert_eq!(make_header().required_size(), 16);
    }

    // --- encode / decode round-trip ---

    #[test]
    fn encode_decode_round_trip() {
        let h = make_header();
        let buf = encode_header(&h);
        let decoded = Header::decode(&mut &buf[..]).unwrap();
        assert_eq!(decoded, h);
    }

    #[test]
    fn encode_returns_16() {
        let h = make_header();
        let mut buf = [0u8; 16];
        let n = h.encode(&mut buf.as_mut_slice()).unwrap();
        assert_eq!(n, 16);
    }

    #[test]
    fn sd_header_round_trips() {
        let h = Header::new_sd(0x0000_0042, 28);
        let buf = encode_header(&h);
        let decoded = Header::decode(&mut &buf[..]).unwrap();
        assert_eq!(decoded, h);
    }

    // --- decode error paths ---

    #[test]
    fn decode_invalid_protocol_version_returns_error() {
        let mut h = make_header();
        h.protocol_version = 0x02;
        // Manually encode with wrong protocol version
        let mid = h.message_id.message_id().to_be_bytes();
        let len = h.length.to_be_bytes();
        let rid = h.request_id.to_be_bytes();
        let buf: [u8; 16] = [
            mid[0], mid[1], mid[2], mid[3], len[0], len[1], len[2], len[3], rid[0], rid[1], rid[2],
            rid[3], 0x02, // bad protocol version
            0x03, 0x00, 0x00,
        ];
        assert!(matches!(
            Header::decode(&mut &buf[..]),
            Err(Error::InvalidProtocolVersion(0x02))
        ));
    }

    #[test]
    fn decode_invalid_message_type_returns_error() {
        let h = make_header();
        let mut buf = encode_header(&h);
        buf[14] = 0xFF; // invalid message type
        assert!(matches!(
            Header::decode(&mut &buf[..]),
            Err(Error::InvalidMessageTypeField(0xFF))
        ));
    }

    #[test]
    fn decode_invalid_return_code_returns_error() {
        let h = make_header();
        let mut buf = encode_header(&h);
        buf[15] = 0x5F; // invalid return code
        assert!(matches!(
            Header::decode(&mut &buf[..]),
            Err(Error::InvalidReturnCode(0x5F))
        ));
    }

    #[test]
    fn decode_truncated_input_returns_eof() {
        let buf: [u8; 4] = [0x00, 0x00, 0x00, 0x00];
        assert!(matches!(
            Header::decode(&mut &buf[..]),
            Err(Error::UnexpectedEof)
        ));
    }
}

impl WireFormat for Header {
    fn decode<T: embedded_io::Read>(reader: &mut T) -> Result<Self, Error> {
        let message_id = MessageId::from(reader.read_u32_be()?);
        let length = reader.read_u32_be()?;
        let request_id = reader.read_u32_be()?;
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
            request_id,
            protocol_version,
            interface_version,
            message_type,
            return_code,
        })
    }

    fn required_size(&self) -> usize {
        16
    }

    fn encode<T: embedded_io::Write>(&self, writer: &mut T) -> Result<usize, Error> {
        writer.write_u32_be(self.message_id.message_id())?;
        writer.write_u32_be(self.length)?;
        writer.write_u32_be(self.request_id)?;
        writer.write_u8(self.protocol_version)?;
        writer.write_u8(self.interface_version)?;
        writer.write_u8(u8::from(self.message_type))?;
        writer.write_u8(u8::from(self.return_code))?;
        Ok(16)
    }
}
