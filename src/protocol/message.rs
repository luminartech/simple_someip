use crate::{
    protocol::{Error, Header, MessageType, ReturnCode, header::HeaderView, sd::SdHeaderView},
    traits::PayloadWireFormat,
};
use automotive_wire_codec::{Decode, Encode};

/// A SOME/IP message consisting of a [`Header`] and a payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Message<PayloadDefinition> {
    header: Header,
    payload: PayloadDefinition,
}

impl<PayloadDefinition: PayloadWireFormat> Message<PayloadDefinition> {
    /// Creates a new message from a header and payload.
    pub const fn new(header: Header, payload: PayloadDefinition) -> Self {
        Self { header, payload }
    }

    /// Creates a new SOME/IP-SD message from a request ID and SD header.
    ///
    /// # Errors
    ///
    /// Returns the error from [`Encode::encoded_size`] on the SD header. No
    /// in-tree `SdHeader` can fail this -- `sd::Header::encoded_size` is
    /// unconditionally `Ok` -- but [`PayloadWireFormat`] is public, and a
    /// downstream implementation may.
    pub fn new_sd(
        request_id: u32,
        sd_header: &<PayloadDefinition as PayloadWireFormat>::SdHeader,
    ) -> Result<Self, Error> {
        // Propagated rather than defaulted. `unwrap_or(0)` produced
        // `Header::new_sd(request_id, 0)` -- a header declaring the bare
        // 8-byte SD length -- and `encode` then wrote the full payload after
        // it. Receivers truncate at the declared length, so a failure here
        // used to become silent wire corruption instead of an error.
        let sd_header_size = sd_header.encoded_size()?;
        Ok(Self::new(
            Header::new_sd(request_id, sd_header_size),
            PayloadDefinition::new_sd_payload(sd_header),
        ))
    }

    /// Returns a reference to the message header.
    pub const fn header(&self) -> &Header {
        &self.header
    }

    /// Returns `true` if this is a SOME/IP-SD message.
    pub const fn is_sd(&self) -> bool {
        self.header.is_sd()
    }

    /// Sets the request ID in the header.
    pub const fn set_request_id(&mut self, request_id: u32) {
        self.header.set_request_id(request_id);
    }

    /// Returns the SD header if this is an SD message, or `None` otherwise.
    pub fn sd_header(&self) -> Option<&<PayloadDefinition as PayloadWireFormat>::SdHeader> {
        if !self.header().message_id().is_sd() || self.header().message_type().is_tp() {
            return None;
        }
        self.payload.as_sd_header()
    }

    /// Returns a reference to the payload.
    pub const fn payload(&self) -> &PayloadDefinition {
        &self.payload
    }

    /// Returns a mutable reference to the payload.
    pub const fn payload_mut(&mut self) -> &mut PayloadDefinition {
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
    ///
    /// # Errors
    ///
    /// Returns an error if the header is invalid, the buffer is too short for the
    /// declared payload, or SD-specific validation fails.
    ///
    /// Any bytes past the declared payload are silently discarded. Use the
    /// [`Decode`] impl's [`decode`](Decode::decode) to recover the trailing
    /// bytes (the next message in a multi-message datagram), or
    /// [`decode_exact`](Decode::decode_exact) to reject them.
    ///
    /// This is a thin wrapper over the [`Decode`] impl, which is the single
    /// source of decode logic for this type.
    pub fn parse(buf: &'a [u8]) -> Result<Self, Error> {
        Ok(Self::decode(buf)?.0)
    }

    /// Returns the header view.
    #[must_use]
    pub fn header(&self) -> HeaderView<'a> {
        self.header
    }

    /// Returns the raw payload bytes.
    #[must_use]
    pub fn payload_bytes(&self) -> &'a [u8] {
        self.payload
    }

    /// Returns `true` if this is a SOME/IP-SD message.
    #[must_use]
    pub fn is_sd(&self) -> bool {
        self.header.is_sd()
    }

    /// Parse the payload as an SD header.
    /// The caller should check `is_sd()` first; this method returns an error
    /// if the message is not an SD message (the SD validation in `parse` must
    /// have already passed).
    ///
    /// # Errors
    ///
    /// Returns an error if this is not an SD message or the SD payload is malformed.
    pub fn sd_header(&self) -> Result<SdHeaderView<'a>, Error> {
        if !self.is_sd() {
            return Err(crate::protocol::sd::Error::InvalidMessage("Not an SD message").into());
        }
        SdHeaderView::parse(self.payload)
    }
}

impl<'a> Decode<'a> for MessageView<'a> {
    type Error = Error;

    /// Decode a single SOME/IP message from the front of `buf`.
    ///
    /// Validates the header, checks that the buffer contains enough data for the
    /// declared payload, and for SD messages validates SD-specific constraints.
    /// Returns `(message, remaining_bytes)`, where the remainder is any bytes
    /// past this message's declared payload (the next message in a
    /// multi-message datagram).
    ///
    /// # Errors
    ///
    /// Returns an error if the header is invalid, the buffer is too short for the
    /// declared payload, or SD-specific validation fails.
    fn decode(buf: &'a [u8]) -> Result<(Self, &'a [u8]), Error> {
        let (header, remaining) = HeaderView::decode(buf)?;
        if header.length() < 8 {
            return Err(Error::InvalidLength(header.length()));
        }
        let payload_size = header.payload_size();

        if remaining.len() < payload_size {
            return Err(automotive_wire_codec::Incomplete {
                needed: payload_size,
                available: remaining.len(),
            }
            .into());
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
        let rest = &remaining[payload_size..];
        Ok((Self { header, payload }, rest))
    }
}

impl<PayloadDefinition: PayloadWireFormat> Encode for Message<PayloadDefinition> {
    type Error = Error;

    fn encoded_size(&self) -> Result<usize, Self::Error> {
        Ok(self.header.encoded_size()? + self.payload.encoded_size()?)
    }

    fn encode(&self, writer: &mut impl embedded_io::Write) -> Result<usize, Error> {
        Ok(self.header.encode(writer)? + self.payload.encode(writer)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::sd::test_support::{TestPayload, TestSdHeader, empty_sd_header};
    use crate::protocol::{MessageId, sd, sd::RebootFlag};

    type Msg = Message<TestPayload>;

    fn minimal_sd_header() -> TestSdHeader {
        empty_sd_header()
    }

    fn make_sd_message() -> Msg {
        Msg::new_sd(0x0000_0001, &minimal_sd_header()).expect("in-tree SdHeader cannot fail")
    }

    /// A failing `SdHeader::encoded_size` must surface as an error, not as a
    /// header declaring the bare 8-byte SD length.
    ///
    /// `unwrap_or(0)` built `Header::new_sd(request_id, 0)` on `Err`, and
    /// `Message::encode` then wrote the full payload after it. Receivers
    /// truncate at the declared length, so the failure mode was silent wire
    /// corruption rather than an error. (PR #153 review.)
    #[test]
    fn new_sd_surfaces_a_failing_sd_header_size() {
        use crate::protocol::sd::test_support::{FailingPayload, FailingSdHeader};

        assert!(
            FailingSdHeader.encoded_size().is_err(),
            "fixture must actually fail, or this test proves nothing",
        );
        assert!(Message::<FailingPayload>::new_sd(0x1, &FailingSdHeader).is_err());
    }

    // --- new ---

    #[test]
    fn new_stores_header_and_payload() {
        let header = Header::new_sd(0x42, 12);
        let payload = TestPayload::new_sd_payload(&minimal_sd_header());
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

    // --- Encode: encoded_size ---

    #[test]
    fn required_size_is_header_plus_payload() {
        let msg = make_sd_message();
        let expected = msg.header().encoded_size().unwrap() + msg.payload().encoded_size().unwrap();
        assert_eq!(msg.encoded_size().unwrap(), expected);
    }

    // --- Encode: encode / MessageView::parse round-trip ---

    #[test]
    fn encode_parse_round_trip() {
        let msg = make_sd_message();
        let mut buf = [0u8; 64];
        let n = msg.encode(&mut buf.as_mut_slice()).unwrap();
        assert_eq!(n, msg.encoded_size().unwrap());
        let view = MessageView::parse(&buf[..n]).unwrap();
        assert!(view.is_sd());
        assert_eq!(view.header().to_owned(), *msg.header());
    }

    #[test]
    fn encode_parse_with_entries() {
        let mut entries = heapless::Vec::<sd::Entry, 4>::new();
        entries
            .push(sd::Entry::FindService(sd::ServiceEntry::find(0xABCD)))
            .unwrap();
        let sd_hdr = TestSdHeader {
            flags: sd::Flags::new_sd(RebootFlag::RecentlyRebooted),
            entries,
            options: heapless::Vec::new(),
        };
        let msg = Msg::new_sd(0x42, &sd_hdr).expect("in-tree SdHeader cannot fail");
        let mut buf = [0u8; 64];
        let n = msg.encode(&mut buf.as_mut_slice()).unwrap();
        let view = MessageView::parse(&buf[..n]).unwrap();
        let sd_view = view.sd_header().unwrap();
        assert_eq!(sd_view.entry_count(), 1);
        let entry = sd_view.entries().next().unwrap();
        assert_eq!(entry.service_id(), 0xABCD);
    }

    // --- Decode: trailing bytes are the next message ---

    #[test]
    fn decode_returns_trailing_bytes_as_remainder() {
        let msg = make_sd_message();
        let mut buf = [0u8; 128];
        let n = msg.encode(&mut buf.as_mut_slice()).unwrap();
        // Append 5 trailing bytes past the message.
        for (i, b) in [0xDE, 0xAD, 0xBE, 0xEF, 0x42].into_iter().enumerate() {
            buf[n + i] = b;
        }
        let (view, rest) = MessageView::decode(&buf[..n + 5]).unwrap();
        assert_eq!(view.header().to_owned(), *msg.header());
        assert_eq!(rest, &[0xDE, 0xAD, 0xBE, 0xEF, 0x42]);
    }

    #[test]
    fn parse_silently_discards_trailing_bytes() {
        let msg = make_sd_message();
        let mut buf = [0u8; 128];
        let n = msg.encode(&mut buf.as_mut_slice()).unwrap();
        buf[n] = 0xFF;
        // parse (the thin wrapper) drops the remainder without error.
        let view = MessageView::parse(&buf[..=n]).unwrap();
        assert_eq!(view.header().to_owned(), *msg.header());
    }

    #[test]
    fn decode_exact_rejects_trailing_bytes() {
        let msg = make_sd_message();
        let mut buf = [0u8; 128];
        let n = msg.encode(&mut buf.as_mut_slice()).unwrap();
        buf[n] = 0xFF;
        assert!(matches!(
            MessageView::decode_exact(&buf[..=n]),
            Err(Error::Trailing(_))
        ));
        // Exactly-sized succeeds.
        assert!(MessageView::decode_exact(&buf[..n]).is_ok());
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
            Err(Error::Incomplete(automotive_wire_codec::Incomplete {
                needed: 16,
                available: 4,
            }))
        ));
    }

    #[test]
    fn parse_payload_truncated_reports_needed_and_available() {
        let msg = make_sd_message();
        let mut buf = [0u8; 64];
        let n = msg.encode(&mut buf.as_mut_slice()).unwrap();
        let payload_size = msg.header().payload_size();
        // Keep the full 16-byte header but chop one byte off the payload.
        let short = &buf[..n - 1];
        assert!(matches!(
            MessageView::parse(short),
            Err(Error::Incomplete(automotive_wire_codec::Incomplete {
                needed,
                available,
            })) if needed == payload_size && available == payload_size - 1
        ));
    }

    #[test]
    fn decode_rejects_length_below_8() {
        let msg = make_sd_message();
        let mut buf = [0u8; 64];
        msg.encode(&mut buf.as_mut_slice()).unwrap();
        // Overwrite the length field (bytes 4..8) with a value below the
        // 8-byte minimum. This must be rejected, not underflow/panic.
        let bad_len: u32 = 4;
        buf[4..8].copy_from_slice(&bad_len.to_be_bytes());
        assert!(matches!(
            MessageView::decode(&buf[..]),
            Err(Error::InvalidLength(4))
        ));
        assert!(matches!(
            MessageView::decode_exact(&buf[..16]),
            Err(Error::InvalidLength(4))
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
