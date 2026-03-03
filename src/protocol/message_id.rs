use super::sd;

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
    pub const SD: Self = Self::new(sd::MESSAGE_ID_VALUE);

    /// Create a new `MessageId` directly.
    #[must_use]
    pub const fn new(message_id: u32) -> Self {
        MessageId(message_id)
    }

    /// Create a new `MessageId` from service and method IDs.
    #[must_use]
    pub const fn new_from_service_and_method(service_id: u16, method_id: u16) -> Self {
        MessageId(((service_id as u32) << 16) | method_id as u32)
    }

    /// Get the message ID
    #[inline]
    #[must_use]
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
    #[must_use]
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
    #[must_use]
    pub const fn method_id(&self) -> u16 {
        (self.0 & 0xFFFF) as u16
    }

    /// Set the method ID
    #[inline]
    pub const fn set_method_id(&mut self, method_id: u16) {
        self.0 = (self.0 & 0xFFFF_0000) | method_id as u32;
    }

    /// Message is Event/Notification
    #[inline]
    #[must_use]
    pub const fn is_event(&self) -> bool {
        self.method_id() & 0x8000 != 0
    }

    /// Message is SOME/IP Service Discovery
    #[inline]
    #[must_use]
    pub const fn is_sd(&self) -> bool {
        self.0 == sd::MESSAGE_ID_VALUE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- constructors ---

    #[test]
    fn from_u32() {
        let mid = MessageId::from(0x1234_5678);
        assert_eq!(mid.message_id(), 0x1234_5678);
    }

    #[test]
    fn new() {
        let mid = MessageId::new(0xABCD_EF01);
        assert_eq!(mid.message_id(), 0xABCD_EF01);
    }

    #[test]
    fn new_from_service_and_method() {
        let mid = MessageId::new_from_service_and_method(0x1234, 0x5678);
        assert_eq!(mid.message_id(), 0x1234_5678);
        assert_eq!(mid.service_id(), 0x1234);
        assert_eq!(mid.method_id(), 0x5678);
    }

    // --- getters / setters ---

    #[test]
    fn set_message_id() {
        let mut mid = MessageId::new(0);
        mid.set_message_id(0xDEAD_BEEF);
        assert_eq!(mid.message_id(), 0xDEAD_BEEF);
    }

    #[test]
    fn set_service_id_preserves_method() {
        let mut mid = MessageId::new_from_service_and_method(0x0001, 0x00FF);
        mid.set_service_id(0xAAAA);
        assert_eq!(mid.service_id(), 0xAAAA);
        assert_eq!(mid.method_id(), 0x00FF);
    }

    #[test]
    fn set_method_id_preserves_service() {
        let mut mid = MessageId::new_from_service_and_method(0x00FF, 0x0001);
        mid.set_method_id(0xBBBB);
        assert_eq!(mid.service_id(), 0x00FF);
        assert_eq!(mid.method_id(), 0xBBBB);
    }

    // --- is_event ---

    #[test]
    fn is_event_true_when_method_high_bit_set() {
        let mid = MessageId::new_from_service_and_method(0x0001, 0x8001);
        assert!(mid.is_event());
    }

    #[test]
    fn is_event_false_when_method_high_bit_clear() {
        let mid = MessageId::new_from_service_and_method(0x0001, 0x0001);
        assert!(!mid.is_event());
    }

    // --- is_sd ---

    #[test]
    fn is_sd_true_for_sd_constant() {
        assert!(MessageId::SD.is_sd());
    }

    #[test]
    fn is_sd_false_for_other() {
        let mid = MessageId::new(0x0001_0001);
        assert!(!mid.is_sd());
    }

    // --- SD constant ---

    #[test]
    fn sd_constant_value() {
        assert_eq!(MessageId::SD.message_id(), 0xFFFF_8100);
    }

    // --- Debug ---

    #[test]
    fn debug_format() {
        use core::fmt::Write;
        let mid = MessageId::new_from_service_and_method(0x1234, 0x0001);
        let mut buf = heapless::String::<128>::new();
        write!(buf, "{mid:?}").unwrap();
        assert!(buf.contains("service_id"));
        assert!(buf.contains("method_id"));
    }
}

impl core::fmt::Debug for MessageId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "Message Id: {{ service_id: {:#02X}, method_id: {:#02X} }}",
            self.service_id(),
            self.method_id(),
        )
    }
}
