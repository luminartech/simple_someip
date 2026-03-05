use crate::{
    protocol::{Error, MessageId, MessageTypeField, ReturnCode, byte_order::WriteBytesExt},
    traits::WireFormat,
};

/// SOME/IP header
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Header {
    /// Message ID, encoding service ID and method ID
    message_id: MessageId,
    /// Length of the message in bytes, starting at the request Id
    /// Total length of the message is therefore length + 8
    length: u32,
    /// SOME/IP Request ID (4 bytes): Client ID [31:16] + Session ID [15:0].
    request_id: u32,
    protocol_version: u8,
    interface_version: u8,
    message_type: MessageTypeField,
    return_code: ReturnCode,
}

impl Header {
    /// Returns the message ID (service ID + method ID).
    #[must_use]
    pub const fn message_id(&self) -> MessageId {
        self.message_id
    }

    /// Returns the length field (payload size + 8).
    #[must_use]
    pub const fn length(&self) -> u32 {
        self.length
    }

    /// Returns the request ID (client ID + session ID).
    #[must_use]
    pub const fn request_id(&self) -> u32 {
        self.request_id
    }

    /// Returns the protocol version.
    #[must_use]
    pub const fn protocol_version(&self) -> u8 {
        self.protocol_version
    }

    /// Returns the interface version.
    #[must_use]
    pub const fn interface_version(&self) -> u8 {
        self.interface_version
    }

    /// Returns the message type field.
    #[must_use]
    pub const fn message_type(&self) -> MessageTypeField {
        self.message_type
    }

    /// Returns the return code.
    #[must_use]
    pub const fn return_code(&self) -> ReturnCode {
        self.return_code
    }

    /// Return the 8-byte "upper header" used by E2E UPPER-HEADER-BITS-TO-SHIFT.
    ///
    /// Layout (big-endian): `request_id(4)` + `protocol_version(1)` + `interface_version(1)`
    ///                      + `message_type(1)` + `return_code(1)`
    ///
    /// Note: `request_id` is the full 4-byte SOME/IP Request ID field
    /// (Client ID \[31:16\] + Session ID \[15:0\]), not just the 2-byte Session ID.
    #[must_use]
    pub const fn upper_header_bytes(&self) -> [u8; 8] {
        let rid = self.request_id.to_be_bytes();
        [
            rid[0],
            rid[1],
            rid[2],
            rid[3],
            self.protocol_version,
            self.interface_version,
            self.message_type.as_u8(),
            self.return_code.as_u8(),
        ]
    }

    /// Creates a header from raw field values.
    ///
    /// Unlike [`new`](Self::new), the `length` field is taken directly rather
    /// than being computed from a payload size.  This is the inverse of the
    /// accessor methods and is useful for FFI or any context where the caller
    /// already has the raw on-wire field values.
    #[must_use]
    pub const fn from_fields(
        message_id: MessageId,
        length: u32,
        request_id: u32,
        protocol_version: u8,
        interface_version: u8,
        message_type: MessageTypeField,
        return_code: ReturnCode,
    ) -> Self {
        Self {
            message_id,
            length,
            request_id,
            protocol_version,
            interface_version,
            message_type,
            return_code,
        }
    }

    /// Creates a new header with the given fields.
    ///
    /// # Panics
    ///
    /// Panics if `payload_len` exceeds `u32::MAX - 8`.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub const fn new(
        message_id: MessageId,
        request_id: u32,
        protocol_version: u8,
        interface_version: u8,
        message_type: MessageTypeField,
        return_code: ReturnCode,
        payload_len: usize,
    ) -> Self {
        assert!(payload_len <= u32::MAX as usize - 8);
        Self {
            message_id,
            length: 8 + payload_len as u32,
            request_id,
            protocol_version,
            interface_version,
            message_type,
            return_code,
        }
    }

    /// Creates a new SOME/IP-SD header with standard SD field values.
    ///
    /// # Panics
    ///
    /// Panics if `sd_header_size` exceeds `u32::MAX - 8`.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub const fn new_sd(request_id: u32, sd_header_size: usize) -> Self {
        assert!(sd_header_size <= u32::MAX as usize - 8);
        Self {
            message_id: MessageId::SD,
            length: 8 + sd_header_size as u32,
            request_id,
            protocol_version: 0x01,
            interface_version: 0x01,
            message_type: MessageTypeField::new_sd(),
            return_code: ReturnCode::Ok,
        }
    }

    /// Creates a new header for a SOME/IP event notification.
    ///
    /// # Panics
    ///
    /// Panics if `payload_len` exceeds `u32::MAX - 8`.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub const fn new_event(
        service_id: u16,
        event_id: u16,
        request_id: u32,
        protocol_version: u8,
        interface_version: u8,
        payload_len: usize,
    ) -> Self {
        assert!(payload_len <= u32::MAX as usize - 8);
        Self {
            message_id: MessageId::new_from_service_and_method(service_id, event_id),
            length: 8 + payload_len as u32,
            request_id,
            protocol_version,
            interface_version,
            message_type: MessageTypeField::new(crate::protocol::MessageType::Notification, false),
            return_code: ReturnCode::Ok,
        }
    }

    /// Returns `true` if this is a SOME/IP-SD message.
    #[must_use]
    pub const fn is_sd(&self) -> bool {
        self.message_id.is_sd()
    }

    /// Returns the payload size in bytes (`length - 8`).
    #[must_use]
    pub const fn payload_size(&self) -> usize {
        self.length as usize - 8
    }

    /// Sets the request ID field.
    pub const fn set_request_id(&mut self, request_id: u32) {
        self.request_id = request_id;
    }
}

/// Zero-copy view into a 16-byte SOME/IP header in a buffer.
#[derive(Clone, Copy, Debug)]
pub struct HeaderView<'a>(&'a [u8; 16]);

impl<'a> HeaderView<'a> {
    /// Parse and validate a SOME/IP header from the beginning of `buf`.
    /// Returns `(view, remaining_bytes)` on success.
    ///
    /// # Errors
    ///
    /// Returns an error if `buf` is shorter than 16 bytes, the protocol version is
    /// not `0x01`, the message type byte is unrecognized, or the return code is invalid.
    ///
    /// # Panics
    ///
    /// Cannot panic — the `expect` is guarded by a length check above it.
    pub fn parse(buf: &'a [u8]) -> Result<(Self, &'a [u8]), Error> {
        if buf.len() < 16 {
            return Err(Error::UnexpectedEof);
        }
        let header_bytes: &[u8; 16] = buf[..16].try_into().expect("length checked above");
        let view = Self(header_bytes);

        // Validate protocol version
        let pv = view.protocol_version();
        if pv != 0x01 {
            return Err(Error::InvalidProtocolVersion(pv));
        }
        // Validate message type
        MessageTypeField::try_from(header_bytes[14])?;
        // Validate return code
        ReturnCode::try_from(header_bytes[15])?;

        Ok((view, &buf[16..]))
    }

    /// Returns the message ID (service ID + method ID).
    #[must_use]
    pub fn message_id(&self) -> MessageId {
        MessageId::from(u32::from_be_bytes([
            self.0[0], self.0[1], self.0[2], self.0[3],
        ]))
    }

    /// Returns the length field (payload size + 8).
    #[must_use]
    pub fn length(&self) -> u32 {
        u32::from_be_bytes([self.0[4], self.0[5], self.0[6], self.0[7]])
    }

    /// Returns the request ID (client ID + session ID).
    #[must_use]
    pub fn request_id(&self) -> u32 {
        u32::from_be_bytes([self.0[8], self.0[9], self.0[10], self.0[11]])
    }

    /// Returns the payload size in bytes (`length - 8`).
    #[must_use]
    pub fn payload_size(&self) -> usize {
        self.length() as usize - 8
    }

    /// Returns the protocol version.
    #[must_use]
    pub fn protocol_version(&self) -> u8 {
        self.0[12]
    }

    /// Returns the interface version.
    #[must_use]
    pub fn interface_version(&self) -> u8 {
        self.0[13]
    }

    /// Returns the message type field.
    ///
    /// # Panics
    ///
    /// Cannot panic — the value is validated during [`Self::parse`].
    #[must_use]
    pub fn message_type(&self) -> MessageTypeField {
        // Safe: validated in parse()
        MessageTypeField::try_from(self.0[14]).expect("validated in parse")
    }

    /// Returns the return code.
    ///
    /// # Panics
    ///
    /// Cannot panic — the value is validated during [`Self::parse`].
    #[must_use]
    pub fn return_code(&self) -> ReturnCode {
        // Safe: validated in parse()
        ReturnCode::try_from(self.0[15]).expect("validated in parse")
    }

    /// Returns `true` if this is a SOME/IP-SD message.
    #[must_use]
    pub fn is_sd(&self) -> bool {
        self.message_id().is_sd()
    }

    /// Copies the view into an owned [`Header`].
    #[must_use]
    pub fn to_owned(&self) -> Header {
        Header {
            message_id: self.message_id(),
            length: self.length(),
            request_id: self.request_id(),
            protocol_version: self.protocol_version(),
            interface_version: self.interface_version(),
            message_type: self.message_type(),
            return_code: self.return_code(),
        }
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
        let rid = h.request_id().to_be_bytes();
        assert_eq!(ub[0..4], rid);
        assert_eq!(ub[4], h.protocol_version());
        assert_eq!(ub[5], h.interface_version());
        assert_eq!(ub[6], u8::from(h.message_type()));
        assert_eq!(ub[7], u8::from(h.return_code()));
    }

    // --- new_sd ---

    #[test]
    fn new_sd_fields() {
        let h = Header::new_sd(0x0000_0001, 28);
        assert_eq!(h.message_id(), MessageId::SD);
        assert_eq!(h.length(), 8 + 28);
        assert_eq!(h.request_id(), 0x0000_0001);
        assert_eq!(h.protocol_version(), 0x01);
        assert_eq!(h.interface_version(), 0x01);
        assert_eq!(h.return_code(), ReturnCode::Ok);
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
        assert_eq!(h.request_id(), 0xDEAD_BEEF);
    }

    // --- required_size ---

    #[test]
    fn required_size_is_16() {
        assert_eq!(make_header().required_size(), 16);
    }

    // --- encode / parse round-trip ---

    #[test]
    fn encode_parse_round_trip() {
        let h = make_header();
        let buf = encode_header(&h);
        let (view, remaining) = HeaderView::parse(&buf[..]).unwrap();
        assert_eq!(view.to_owned(), h);
        assert!(remaining.is_empty());
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
        let (view, _) = HeaderView::parse(&buf[..]).unwrap();
        assert_eq!(view.to_owned(), h);
    }

    // --- parse with exactly-sized slice ---

    #[test]
    fn parse_exact_size_slice_returns_empty_remainder() {
        let h = make_header();
        let buf = encode_header(&h);
        // buf is exactly 16 bytes — no extra data
        let (view, remaining) = HeaderView::parse(&buf).unwrap();
        assert_eq!(view.to_owned(), h);
        assert!(remaining.is_empty());
    }

    // --- parse error paths ---

    #[test]
    fn parse_invalid_protocol_version_returns_error() {
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
            HeaderView::parse(&buf[..]),
            Err(Error::InvalidProtocolVersion(0x02))
        ));
    }

    #[test]
    fn parse_invalid_message_type_returns_error() {
        let h = make_header();
        let mut buf = encode_header(&h);
        buf[14] = 0xFF; // invalid message type
        assert!(matches!(
            HeaderView::parse(&buf[..]),
            Err(Error::InvalidMessageTypeField(0xFF))
        ));
    }

    #[test]
    fn parse_invalid_return_code_returns_error() {
        let h = make_header();
        let mut buf = encode_header(&h);
        buf[15] = 0x5F; // invalid return code
        assert!(matches!(
            HeaderView::parse(&buf[..]),
            Err(Error::InvalidReturnCode(0x5F))
        ));
    }

    #[test]
    fn parse_truncated_input_returns_eof() {
        let buf: [u8; 4] = [0x00, 0x00, 0x00, 0x00];
        assert!(matches!(
            HeaderView::parse(&buf[..]),
            Err(Error::UnexpectedEof)
        ));
    }

    // --- HeaderView accessors ---

    #[test]
    fn header_view_accessors() {
        let h = make_header();
        let buf = encode_header(&h);
        let (view, _) = HeaderView::parse(&buf[..]).unwrap();
        assert_eq!(view.message_id(), h.message_id());
        assert_eq!(view.length(), h.length());
        assert_eq!(view.request_id(), h.request_id());
        assert_eq!(view.payload_size(), h.payload_size());
        assert_eq!(view.protocol_version(), h.protocol_version());
        assert_eq!(view.interface_version(), h.interface_version());
        assert_eq!(view.message_type(), h.message_type());
        assert_eq!(view.return_code(), h.return_code());
        assert_eq!(view.is_sd(), h.is_sd());
    }
}

impl WireFormat for Header {
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
