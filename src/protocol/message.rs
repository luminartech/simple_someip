use crate::{
    protocol::{Error, Header, MessageType, ReturnCode, header::HeaderView, sd::SdHeaderView},
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

    pub fn sd_header(&self) -> Option<&<PayloadDefinition as PayloadWireFormat>::SdHeader> {
        if !self.header().message_id().is_sd() || self.header().message_type().is_tp() {
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

/// Zero-copy view into a complete SOME/IP message (header + payload).
#[derive(Clone, Copy, Debug)]
pub struct MessageView<'a> {
    header: HeaderView<'a>,
    payload: &'a [u8],
}

impl<'a> MessageView<'a> {
    /// Parse a complete SOME/IP message from `buf`.
    ///
    /// Validates the header, checks that the buffer contains enough data for
    /// the declared payload, and for SD messages validates SD-specific constraints.
    pub fn parse(buf: &'a [u8]) -> Result<Self, Error> {
        let (header, remaining) = HeaderView::parse(buf)?;
        let payload_size = header.payload_size();

        if remaining.len() < payload_size {
            return Err(Error::UnexpectedEof);
        }

        // SD-specific validation
        if header.is_sd() {
            if payload_size < 12 {
                return Err(
                    crate::protocol::sd::Error::InvalidMessage("SD message too short").into(),
                );
            }
            if header.interface_version() != 0x01 {
                return Err(crate::protocol::sd::Error::InvalidMessage(
                    "SD interface version mismatch",
                )
                .into());
            }
            if header.message_type().message_type() != MessageType::Notification {
                return Err(
                    crate::protocol::sd::Error::InvalidMessage("SD message type mismatch").into(),
                );
            }
            if header.return_code() != ReturnCode::Ok {
                return Err(
                    crate::protocol::sd::Error::InvalidMessage("SD return code mismatch").into(),
                );
            }
        }

        let payload = &remaining[..payload_size];
        Ok(Self { header, payload })
    }

    #[must_use]
    pub fn header(&self) -> HeaderView<'a> {
        self.header
    }

    #[must_use]
    pub fn payload_bytes(&self) -> &'a [u8] {
        self.payload
    }

    #[must_use]
    pub fn is_sd(&self) -> bool {
        self.header.is_sd()
    }

    /// Parse the payload as an SD header.
    /// The caller should check `is_sd()` first; this method returns an error
    /// if the message is not an SD message (the SD validation in `parse` must
    /// have already passed).
    pub fn sd_header(&self) -> Result<SdHeaderView<'a>, Error> {
        if !self.is_sd() {
            return Err(crate::protocol::sd::Error::InvalidMessage("Not an SD message").into());
        }
        SdHeaderView::parse(self.payload)
    }
}

impl<PayloadDefinition: PayloadWireFormat> WireFormat for Message<PayloadDefinition> {
    fn required_size(&self) -> usize {
        self.header.required_size() + self.payload.required_size()
    }

    fn encode<W: embedded_io::Write>(&self, writer: &mut W) -> Result<usize, Error> {
        Ok(self.header.encode(writer)? + self.payload.encode(writer)?)
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
        assert_eq!(msg.header().message_id(), MessageId::SD);
    }

    // --- header / payload / payload_mut ---

    #[test]
    fn header_returns_reference() {
        let msg = make_sd_message();
        assert_eq!(msg.header().protocol_version(), 0x01);
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
        assert_eq!(msg.header().request_id(), 0xDEAD_BEEF);
    }

    // --- get_sd_header ---

    #[test]
    fn get_sd_header_returns_some_for_sd() {
        let sd_hdr = minimal_sd_header();
        let msg = make_sd_message();
        assert_eq!(msg.sd_header().unwrap(), &sd_hdr);
    }

    // --- WireFormat: required_size ---

    #[test]
    fn required_size_is_header_plus_payload() {
        let msg = make_sd_message();
        let expected = msg.header().required_size() + msg.payload().required_size();
        assert_eq!(msg.required_size(), expected);
    }

    // --- WireFormat: encode / MessageView::parse round-trip ---

    #[test]
    fn encode_parse_round_trip() {
        let msg = make_sd_message();
        let mut buf = [0u8; 64];
        let n = msg.encode(&mut buf.as_mut_slice()).unwrap();
        assert_eq!(n, msg.required_size());
        let view = MessageView::parse(&buf[..n]).unwrap();
        assert!(view.is_sd());
        assert_eq!(view.header().to_owned(), *msg.header());
    }

    #[test]
    fn encode_parse_with_entries() {
        let sd_hdr: sd::Header<1, 0> = sd::Header::new_find_services(true, &[0xABCD]);
        let msg = Message::<DiscoveryOnlyPayload<1, 0>>::new_sd(0x42, &sd_hdr);
        let mut buf = [0u8; 64];
        let n = msg.encode(&mut buf.as_mut_slice()).unwrap();
        let view = MessageView::parse(&buf[..n]).unwrap();
        let sd_view = view.sd_header().unwrap();
        let decoded: sd::Header<1, 0> = sd_view.to_owned().unwrap();
        assert_eq!(decoded, sd_hdr);
    }

    // --- parse with exactly-sized slice ---

    #[test]
    fn parse_exact_size_slice_succeeds() {
        let msg = make_sd_message();
        let mut buf = [0u8; 64];
        let n = msg.encode(&mut buf.as_mut_slice()).unwrap();
        // Pass exactly n bytes — no extra data beyond the message
        let view = MessageView::parse(&buf[..n]).unwrap();
        assert!(view.is_sd());
        assert_eq!(view.header().to_owned(), *msg.header());
    }

    // --- parse error paths ---

    #[test]
    fn parse_truncated_returns_eof() {
        let buf: [u8; 4] = [0; 4];
        assert!(matches!(
            MessageView::parse(&buf[..]),
            Err(Error::UnexpectedEof)
        ));
    }

    // --- parse SD validation errors ---

    #[test]
    fn parse_sd_payload_too_short_returns_error() {
        let msg = make_sd_message();
        let mut buf = [0u8; 64];
        msg.encode(&mut buf.as_mut_slice()).unwrap();
        // Overwrite the length field (bytes 4..8) to make payload_size < 12
        // length = 8 + payload_size, so length=19 → payload_size=11
        let bad_len: u32 = 19;
        buf[4..8].copy_from_slice(&bad_len.to_be_bytes());
        assert!(matches!(
            MessageView::parse(&buf[..]),
            Err(Error::Sd(crate::protocol::sd::Error::InvalidMessage(
                "SD message too short"
            )))
        ));
    }

    #[test]
    fn parse_sd_wrong_interface_version_returns_error() {
        let msg = make_sd_message();
        let mut buf = [0u8; 64];
        let n = msg.encode(&mut buf.as_mut_slice()).unwrap();
        buf[13] = 0x02; // interface_version at byte 13
        assert!(matches!(
            MessageView::parse(&buf[..n]),
            Err(Error::Sd(crate::protocol::sd::Error::InvalidMessage(
                "SD interface version mismatch"
            )))
        ));
    }

    #[test]
    fn parse_sd_wrong_message_type_returns_error() {
        let msg = make_sd_message();
        let mut buf = [0u8; 64];
        let n = msg.encode(&mut buf.as_mut_slice()).unwrap();
        buf[14] = 0x00; // Request instead of Notification
        assert!(matches!(
            MessageView::parse(&buf[..n]),
            Err(Error::Sd(crate::protocol::sd::Error::InvalidMessage(
                "SD message type mismatch"
            )))
        ));
    }

    #[test]
    fn parse_sd_wrong_return_code_returns_error() {
        let msg = make_sd_message();
        let mut buf = [0u8; 64];
        let n = msg.encode(&mut buf.as_mut_slice()).unwrap();
        buf[15] = 0x01; // NotOk instead of Ok
        assert!(matches!(
            MessageView::parse(&buf[..n]),
            Err(Error::Sd(crate::protocol::sd::Error::InvalidMessage(
                "SD return code mismatch"
            )))
        ));
    }

    // --- MessageView accessors ---

    #[test]
    fn message_view_payload_bytes() {
        let msg = make_sd_message();
        let mut buf = [0u8; 64];
        let n = msg.encode(&mut buf.as_mut_slice()).unwrap();
        let view = MessageView::parse(&buf[..n]).unwrap();
        assert_eq!(view.payload_bytes().len(), msg.header().payload_size());
    }

    #[test]
    fn message_view_sd_header_on_non_sd_returns_error() {
        // Build a non-SD message
        let header = Header::new(
            MessageId::new_from_service_and_method(0x1234, 0x0001),
            0x0001,
            0x01,
            0x01,
            crate::protocol::MessageTypeField::try_from(0x00).unwrap(),
            ReturnCode::Ok,
            0,
        );
        let mut buf = [0u8; 16];
        header.encode(&mut buf.as_mut_slice()).unwrap();
        let view = MessageView::parse(&buf).unwrap();
        assert!(matches!(
            view.sd_header(),
            Err(Error::Sd(crate::protocol::sd::Error::InvalidMessage(
                "Not an SD message"
            )))
        ));
    }
}
