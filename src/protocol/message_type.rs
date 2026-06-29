use super::Error;

/// Bit flag in `message_type` field indicating that the message is a SOME/IP TP message.
pub const MESSAGE_TYPE_TP_FLAG: u8 = 0x20;

///Message types of a SOME/IP message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageType {
    /// A request expecting a response.
    Request,
    /// A fire-and-forget request.
    RequestNoReturn,
    /// An event notification.
    Notification,
    /// A response to a request.
    Response,
    /// An error response.
    Error,
}

impl MessageType {
    const fn try_from(value: u8) -> Result<Self, Error> {
        match value & !MESSAGE_TYPE_TP_FLAG {
            0x00 => Ok(MessageType::Request),
            0x01 => Ok(MessageType::RequestNoReturn),
            0x02 => Ok(MessageType::Notification),
            0x80 => Ok(MessageType::Response),
            0x81 => Ok(MessageType::Error),
            _ => Err(Error::InvalidMessageTypeField(value)),
        }
    }
}

impl TryFrom<u8> for MessageType {
    type Error = Error;
    fn try_from(value: u8) -> Result<Self, Error> {
        MessageType::try_from(value)
    }
}

/// Newtype for message type field
/// The field encodes the message type and the TP flag.
/// The TP flag indicates that the message is a SOME/IP TP message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MessageTypeField(u8);

impl TryFrom<u8> for MessageTypeField {
    type Error = Error;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        MessageType::try_from(value)?;
        Ok(MessageTypeField(value))
    }
}

impl From<MessageTypeField> for u8 {
    fn from(message_type_field: MessageTypeField) -> u8 {
        message_type_field.0
    }
}

impl MessageTypeField {
    /// Creates a new message type field from a [`MessageType`] and TP flag.
    #[must_use]
    pub const fn new(msg_type: MessageType, tp: bool) -> Self {
        // Map to the SOME/IP *wire* encoding — NOT `msg_type as u8`, which
        // is the enum discriminant and only coincides with the wire byte
        // for Request/RequestNoReturn/Notification (0/1/2). Response and
        // Error diverge (discriminant 3/4 vs wire 0x80/0x81).
        let base = match msg_type {
            MessageType::Request => 0x00,
            MessageType::RequestNoReturn => 0x01,
            MessageType::Notification => 0x02,
            MessageType::Response => 0x80,
            MessageType::Error => 0x81,
        };
        let message_type_byte = if tp {
            base | MESSAGE_TYPE_TP_FLAG
        } else {
            base
        };
        MessageTypeField(message_type_byte)
    }

    /// Creates a message type field for SOME/IP-SD (Notification, no TP).
    #[must_use]
    pub const fn new_sd() -> Self {
        Self::new(MessageType::Notification, false)
    }

    /// Returns the message type of the message
    ///
    /// # Panics
    ///
    /// Cannot panic — the inner byte is always a valid `MessageType`.
    #[must_use]
    pub const fn message_type(&self) -> MessageType {
        // The inner byte is always valid because it is validated on construction.
        match self.0 & !MESSAGE_TYPE_TP_FLAG {
            0x00 => MessageType::Request,
            0x01 => MessageType::RequestNoReturn,
            0x02 => MessageType::Notification,
            0x80 => MessageType::Response,
            0x81 => MessageType::Error,
            _ => unreachable!(),
        }
    }

    /// Returns the raw byte value of the message type field.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self.0
    }

    /// Returns `true` if the TP (Transport Protocol) flag is set.
    #[must_use]
    pub const fn is_tp(&self) -> bool {
        self.0 & MESSAGE_TYPE_TP_FLAG != 0
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    // --- MessageType TryFrom<u8> ---

    #[test]
    fn message_type_trait_try_from() {
        // Exercise the TryFrom<u8> trait impl (not the inherent const fn)
        let mt: Result<MessageType, _> = 0x00u8.try_into();
        assert_eq!(mt.unwrap(), MessageType::Request);
    }

    // --- MessageTypeField::new ---

    #[test]
    fn new_with_tp_true() {
        let field = MessageTypeField::new(MessageType::Request, true);
        assert_eq!(field.message_type(), MessageType::Request);
        assert!(field.is_tp());
        assert_eq!(u8::from(field), 0x20);
    }

    #[test]
    fn new_with_tp_false() {
        let field = MessageTypeField::new(MessageType::Request, false);
        assert_eq!(field.message_type(), MessageType::Request);
        assert!(!field.is_tp());
        assert_eq!(u8::from(field), 0x00);
    }

    // --- MessageTypeField::new_sd ---

    #[test]
    fn new_sd_is_notification_no_tp() {
        let field = MessageTypeField::new_sd();
        assert_eq!(field.message_type(), MessageType::Notification);
        assert!(!field.is_tp());
    }

    /// `new` must emit the SOME/IP *wire* byte for every variant — not the
    /// enum discriminant. `Response`/`Error` are the variants where the two
    /// diverge (discriminant 3/4 vs wire 0x80/0x81); the others coincide.
    #[test]
    fn new_emits_wire_byte_for_all_variants() {
        for (mt, wire) in [
            (MessageType::Request, 0x00u8),
            (MessageType::RequestNoReturn, 0x01),
            (MessageType::Notification, 0x02),
            (MessageType::Response, 0x80),
            (MessageType::Error, 0x81),
        ] {
            let field = MessageTypeField::new(mt, false);
            assert_eq!(u8::from(field), wire, "wire byte for {mt:?}");
            assert_eq!(field.message_type(), mt, "round-trips back to {mt:?}");
            let tp = MessageTypeField::new(mt, true);
            assert_eq!(
                u8::from(tp),
                wire | MESSAGE_TYPE_TP_FLAG,
                "wire byte for {mt:?} with TP flag"
            );
            assert_eq!(tp.message_type(), mt, "TP variant round-trips for {mt:?}");
        }
    }

    // --- exhaustive u8 ---

    /// Check that we properly decode and encode hex bytes
    #[test]
    fn test_all_u8_values() {
        let valid_inputs: [u8; 10] = [0x00, 0x01, 0x02, 0x80, 0x81, 0x20, 0x21, 0x22, 0xA0, 0xA1];
        for i in 0..=255 {
            let msg_type = MessageTypeField::try_from(i);
            if valid_inputs.contains(&i) {
                assert!(msg_type.is_ok());
                let msg_type = msg_type.unwrap();
                match i {
                    0x00 => {
                        assert_eq!(msg_type.message_type(), MessageType::Request);
                        assert!(!msg_type.is_tp());
                    }
                    0x01 => {
                        assert_eq!(msg_type.message_type(), MessageType::RequestNoReturn);
                        assert!(!msg_type.is_tp());
                    }
                    0x02 => {
                        assert_eq!(msg_type.message_type(), MessageType::Notification);
                        assert!(!msg_type.is_tp());
                    }
                    0x80 => {
                        assert_eq!(msg_type.message_type(), MessageType::Response);
                        assert!(!msg_type.is_tp());
                    }
                    0x81 => {
                        assert_eq!(msg_type.message_type(), MessageType::Error);
                        assert!(!msg_type.is_tp());
                    }
                    0x20 => {
                        assert_eq!(msg_type.message_type(), MessageType::Request);
                        assert!(msg_type.is_tp());
                    }
                    0x21 => {
                        assert_eq!(msg_type.message_type(), MessageType::RequestNoReturn);
                        assert!(msg_type.is_tp());
                    }
                    0x22 => {
                        assert_eq!(msg_type.message_type(), MessageType::Notification);
                        assert!(msg_type.is_tp());
                    }
                    0xA0 => {
                        assert_eq!(msg_type.message_type(), MessageType::Response);
                        assert!(msg_type.is_tp());
                    }
                    0xA1 => {
                        assert_eq!(msg_type.message_type(), MessageType::Error);
                        assert!(msg_type.is_tp());
                    }

                    _ => unreachable!("Only valid inputs should have made it to this point"),
                }
            } else {
                assert!(msg_type.is_err());
            }
        }
    }
}
