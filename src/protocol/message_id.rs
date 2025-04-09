use crate::SD_MESSAGE_ID_VALUE;

/// Newtype for a message ID.
/// The Message ID is a 32-bit identifier that is unique for each message.
/// It encodes both the service ID and the method ID.
/// Message IDs are assumed to be unique for an entire vehicle network.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct MessageId(u32);

impl From<u32> for MessageId {
    fn from(message_id: u32) -> Self {
        MessageId(message_id)
    }
}

impl MessageId {
    /// Message ID for Service Discovery
    pub const SD: Self = Self::new(SD_MESSAGE_ID_VALUE);

    /// Create a new MessageId directly.
    pub const fn new(message_id: u32) -> Self {
        MessageId(message_id)
    }

    /// Create a new MessageId from service and method IDs.
    pub const fn new_from_service_and_method(service_id: u16, method_id: u16) -> Self {
        MessageId(((service_id as u32) << 16) | method_id as u32)
    }

    /// Get the message ID
    #[inline]
    pub const fn message_id(&self) -> u32 {
        self.0
    }

    /// Set the message ID
    #[inline]
    pub const fn set_message_id(&mut self, message_id: u32) {
        self.0 = message_id;
    }

    /// Get the service ID
    #[inline]
    pub const fn service_id(&self) -> u16 {
        (self.0 >> 16) as u16
    }

    /// Set the service ID
    #[inline]
    pub const fn set_service_id(&mut self, service_id: u16) {
        self.0 = (self.0 & 0xFFFF) | ((service_id as u32) << 16);
    }

    /// Get the method ID
    #[inline]
    pub const fn method_id(&self) -> u16 {
        (self.0 & 0xFFFF) as u16
    }

    /// Set the method ID
    #[inline]
    pub const fn set_method_id(&mut self, method_id: u16) {
        self.0 = (self.0 & 0xFFFF0000) | method_id as u32;
    }

    /// Message is Event/Notification
    #[inline]
    pub const fn is_event(&self) -> bool {
        self.method_id() & 0x8000 != 0
    }

    /// Message is SOME/IP Service Discovery
    #[inline]
    pub const fn is_sd(&self) -> bool {
        self.0 == crate::SD_MESSAGE_ID_VALUE
    }
}

impl std::fmt::Debug for MessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Message Id: {{ service_id: {:#02X}, method_id: {:#02X} }}",
            self.service_id(),
            self.method_id(),
        )
    }
}
