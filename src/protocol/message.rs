use crate::{
    protocol::{Error, Header, MessageType, ReturnCode, byte_order::Take},
    traits::{PayloadWireFormat, WireFormat},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Message<PayloadDefinition> {
    header: Header,
    payload: PayloadDefinition,
}

impl<PayloadDefinition: PayloadWireFormat> Message<PayloadDefinition> {
    pub const fn new(header: Header, payload: PayloadDefinition) -> Self {
        Self { header, payload }
    }

    #[must_use]
    pub fn new_sd(
        request_id: u32,
        sd_header: &<PayloadDefinition as PayloadWireFormat>::SdHeader,
    ) -> Self {
        let sd_header_size = sd_header.required_size();
        Self::new(
            Header::new_sd(request_id, sd_header_size),
            PayloadDefinition::new_sd_payload(sd_header),
        )
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    pub const fn is_sd(&self) -> bool {
        self.header.is_sd()
    }

    pub fn set_request_id(&mut self, request_id: u32) {
        self.header.set_request_id(request_id);
    }

    pub fn get_sd_header(&self) -> Option<&<PayloadDefinition as PayloadWireFormat>::SdHeader> {
        if !self.header().message_id.is_sd() || self.header().message_type.is_tp() {
            return None;
        }
        self.payload.as_sd_header()
    }

    pub fn payload(&self) -> &PayloadDefinition {
        &self.payload
    }

    pub fn payload_mut(&mut self) -> &mut PayloadDefinition {
        &mut self.payload
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{MessageId, sd};
    use crate::traits::DiscoveryOnlyPayload;

    type Msg = Message<DiscoveryOnlyPayload>;

    fn minimal_sd_header() -> sd::Header {
        sd::Header::new_find_services(false, &[])
    }

    fn make_sd_message() -> Msg {
        Msg::new_sd(0x0000_0001, &minimal_sd_header())
    }

    // --- new ---

    #[test]
    fn new_stores_header_and_payload() {
        let header = Header::new_sd(0x42, 12);
        let payload = DiscoveryOnlyPayload::new_sd_payload(&minimal_sd_header());
        let msg = Msg::new(header.clone(), payload.clone());
        assert_eq!(*msg.header(), header);
        assert_eq!(*msg.payload(), payload);
    }

    // --- new_sd ---

    #[test]
    fn new_sd_creates_valid_message() {
        let msg = make_sd_message();
        assert!(msg.is_sd());
        assert_eq!(msg.header().message_id, MessageId::SD);
    }

    // --- header / payload / payload_mut ---

    #[test]
    fn header_returns_reference() {
        let msg = make_sd_message();
        assert_eq!(msg.header().protocol_version, 0x01);
    }

    #[test]
    fn payload_returns_reference() {
        let sd_hdr = minimal_sd_header();
        let msg = make_sd_message();
        assert_eq!(msg.payload().as_sd_header().unwrap(), &sd_hdr);
    }

    #[test]
    fn payload_mut_allows_modification() {
        let mut msg = make_sd_message();
        let _p = msg.payload_mut();
        // Just verify we get a mutable reference without panic
    }

    // --- is_sd ---

    #[test]
    fn is_sd_true_for_sd_message() {
        assert!(make_sd_message().is_sd());
    }

    // --- set_request_id ---

    #[test]
    fn set_request_id_updates_header() {
        let mut msg = make_sd_message();
        msg.set_request_id(0xDEAD_BEEF);
        assert_eq!(msg.header().request_id, 0xDEAD_BEEF);
    }

    // --- get_sd_header ---

    #[test]
    fn get_sd_header_returns_some_for_sd() {
        let sd_hdr = minimal_sd_header();
        let msg = make_sd_message();
        assert_eq!(msg.get_sd_header().unwrap(), &sd_hdr);
    }

    // --- WireFormat: required_size ---

    #[test]
    fn required_size_is_header_plus_payload() {
        let msg = make_sd_message();
        let expected = msg.header().required_size() + msg.payload().required_size();
        assert_eq!(msg.required_size(), expected);
    }

    // --- WireFormat: encode / decode round-trip ---

    #[test]
    fn encode_decode_round_trip() {
        let msg = make_sd_message();
        let mut buf = [0u8; 64];
        let n = msg.encode(&mut buf.as_mut_slice()).unwrap();
        assert_eq!(n, msg.required_size());
        let decoded = Msg::decode(&mut &buf[..n]).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn encode_decode_with_entries() {
        let sd_hdr: sd::Header<1, 0> = sd::Header::new_find_services(true, &[0xABCD]);
        let msg = Message::<DiscoveryOnlyPayload<1, 0>>::new_sd(0x42, &sd_hdr);
        let mut buf = [0u8; 64];
        let n = msg.encode(&mut buf.as_mut_slice()).unwrap();
        let decoded = Message::<DiscoveryOnlyPayload<1, 0>>::decode(&mut &buf[..n]).unwrap();
        assert_eq!(decoded, msg);
    }

    // --- decode error paths ---

    #[test]
    fn decode_truncated_returns_eof() {
        let buf: [u8; 4] = [0; 4];
        assert!(matches!(
            Msg::decode(&mut &buf[..]),
            Err(Error::UnexpectedEof)
        ));
    }

    // --- decode SD validation errors ---

    #[test]
    fn decode_sd_payload_too_short_returns_error() {
        let msg = make_sd_message();
        let mut buf = [0u8; 64];
        msg.encode(&mut buf.as_mut_slice()).unwrap();
        // Overwrite the length field (bytes 4..8) to make payload_size < 12
        // length = 8 + payload_size, so length=19 → payload_size=11
        let bad_len: u32 = 19;
        buf[4..8].copy_from_slice(&bad_len.to_be_bytes());
        assert!(matches!(
            Msg::decode(&mut &buf[..]),
            Err(Error::InvalidSDMessage("SD message too short"))
        ));
    }

    #[test]
    fn decode_sd_wrong_interface_version_returns_error() {
        let msg = make_sd_message();
        let mut buf = [0u8; 64];
        let n = msg.encode(&mut buf.as_mut_slice()).unwrap();
        buf[13] = 0x02; // interface_version at byte 13
        assert!(matches!(
            Msg::decode(&mut &buf[..n]),
            Err(Error::InvalidSDMessage("SD interface version mismatch"))
        ));
    }

    #[test]
    fn decode_sd_wrong_message_type_returns_error() {
        let msg = make_sd_message();
        let mut buf = [0u8; 64];
        let n = msg.encode(&mut buf.as_mut_slice()).unwrap();
        buf[14] = 0x00; // Request instead of Notification
        assert!(matches!(
            Msg::decode(&mut &buf[..n]),
            Err(Error::InvalidSDMessage("SD message type mismatch"))
        ));
    }

    #[test]
    fn decode_sd_wrong_return_code_returns_error() {
        let msg = make_sd_message();
        let mut buf = [0u8; 64];
        let n = msg.encode(&mut buf.as_mut_slice()).unwrap();
        buf[15] = 0x01; // NotOk instead of Ok
        assert!(matches!(
            Msg::decode(&mut &buf[..n]),
            Err(Error::InvalidSDMessage("SD return code mismatch"))
        ));
    }
}

impl<PayloadDefinition: PayloadWireFormat> WireFormat for Message<PayloadDefinition> {
    fn decode<R: embedded_io::Read>(reader: &mut R) -> Result<Self, Error> {
        let header = Header::decode(reader)?;
        if header.message_id.is_sd() {
            if header.payload_size() < 12 {
                return Err(Error::InvalidSDMessage("SD message too short"));
            }
            if header.interface_version != 0x01 {
                return Err(Error::InvalidSDMessage("SD interface version mismatch"));
            }
            if header.message_type.message_type() != MessageType::Notification {
                return Err(Error::InvalidSDMessage("SD message type mismatch"));
            }
            if header.return_code != ReturnCode::Ok {
                return Err(Error::InvalidSDMessage("SD return code mismatch"));
            }
        }
        let mut payload_reader = Take::new(reader, header.payload_size());
        let payload =
            PayloadDefinition::decode_with_message_id(header.message_id, &mut payload_reader)?;
        Ok(Self::new(header, payload))
    }

    fn required_size(&self) -> usize {
        self.header.required_size() + self.payload.required_size()
    }

    fn encode<W: embedded_io::Write>(&self, writer: &mut W) -> Result<usize, Error> {
        Ok(self.header.encode(writer)? + self.payload.encode(writer)?)
    }
}
