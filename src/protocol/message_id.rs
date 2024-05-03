///Message id for SOME/IP service discovery messages
pub const SD_MESSAGE_ID: u32 = 0xffff_8100;

/// Newtype for a message ID.
/// The Message ID is a 32-bit identifier that is unique for each message.
/// It encodes both the service ID and the method ID.
/// Message IDs are assumed to be unique for an entire vehicle network.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MessageId(u32);

/// Implement From<u32> for MessageId
impl From<u32> for MessageId {
    fn from(message_id: u32) -> Self {
        MessageId(message_id)
    }
}

impl MessageId {
    /// Create a new MessageId.
    pub fn new(message_id: u32) -> Self {
        MessageId(message_id)
    }
    pub fn new_sd() -> Self {
        MessageId(SD_MESSAGE_ID)
    }

    /// Get the message ID
    #[inline]
    pub fn message_id(&self) -> u32 {
        self.0
    }

    /// Set the message ID
    #[inline]
    pub fn set_message_id(&mut self, message_id: u32) {
        self.0 = message_id;
    }

    /// Get the service ID
    #[inline]
    pub fn service_id(&self) -> u16 {
        (self.0 >> 16) as u16
    }

    /// Set the service ID
    #[inline]
    pub fn set_service_id(&mut self, service_id: u16) {
        self.0 = (self.0 & 0xFFFF) | ((service_id as u32) << 16);
    }

    /// Get the method ID
    #[inline]
    pub fn method_id(&self) -> u16 {
        (self.0 & 0xFFFF) as u16
    }

    /// Set the method ID
    #[inline]
    pub fn set_method_id(&mut self, method_id: u16) {
        self.0 = (self.0 & 0xFFFF0000) | method_id as u32;
    }

    /// Message is Event/Notification
    #[inline]
    pub fn is_event(&self) -> bool {
        self.method_id() & 0x8000 != 0
    }

    /// Message is SOME/IP Service Discovery
    pub fn is_sd(&self) -> bool {
        self.0 == SD_MESSAGE_ID
    }
}
