use super::Error;

/// Bit flag in message_type field indicating that the message is a SOME/IP TP message.
pub const MESSAGE_TYPE_TP_FLAG: u8 = 0x20;

///Message types of a SOME/IP message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MessageType {
    Request,
    RequestNoReturn,
    Notification,
    Response,
    Error,
}

impl TryFrom<u8> for MessageType {
    type Error = Error;
    fn try_from(value: u8) -> Result<Self, Error> {
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
    pub fn new(msg_type: MessageType, tp: bool) -> Self {
        let message_type_byte = if tp {
            msg_type as u8 | MESSAGE_TYPE_TP_FLAG
        } else {
            msg_type as u8
        };
        MessageTypeField(message_type_byte)
    }

    pub fn new_sd() -> Self {
        Self::new(MessageType::Notification, false)
    }

    /// Returns the message type of the message
    pub fn message_type(&self) -> MessageType {
        // This unwrap is safe because the private message_type_byte is always a valid MessageType
        MessageType::try_from(self.0).unwrap()
    }

    pub fn is_tp(&self) -> bool {
        self.0 & MESSAGE_TYPE_TP_FLAG != 0
    }
}

#[cfg(test)]
mod tests {

    use super::*;
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
