#[cfg(feature = "std")]
use crate::protocol::sd;
use crate::protocol::{self, MessageId, sd::Flags};

/// Information about a service endpoint extracted from an SD message.
#[cfg(feature = "std")]
pub struct OfferedEndpoint {
    /// The SOME/IP service ID.
    pub service_id: u16,
    /// The SOME/IP instance ID.
    pub instance_id: u16,
    /// The major version of the offered service interface.
    pub major_version: u8,
    /// The minor version of the offered service interface.
    pub minor_version: u32,
    /// The IPv4 socket address extracted from the SD options, if present.
    pub addr: Option<std::net::SocketAddrV4>,
    /// `true` for `OfferService`, `false` for `StopOfferService`.
    pub is_offer: bool,
}

/// A trait for types that can be serialized to a [`Writer`](embedded_io::Write).
///
/// `WireFormat` acts as the base trait for all types that can be serialized
/// as part of the Simple SOME/IP ecosystem. Decoding is handled by zero-copy
/// view types (`HeaderView`, `MessageView`, etc.) instead of this trait.
pub trait WireFormat: Send + Sized + Sync {
    /// Returns the number of bytes required to serialize this value.
    fn required_size(&self) -> usize;

    /// Serialize a value to a byte stream.
    /// Returns the number of bytes written.
    /// # Errors
    /// - If the data cannot be written to the stream
    fn encode<T: embedded_io::Write>(&self, writer: &mut T) -> Result<usize, protocol::Error>;
}

/// A trait for SOME/IP Payload types that can be serialized to a
/// [`Writer`](embedded_io::Write) and constructed from raw payload bytes.
///
/// Note that SOME/IP payloads are not self identifying, so the [Message ID](protocol::MessageId)
/// must be provided by the caller.
pub trait PayloadWireFormat: core::fmt::Debug + Send + Sized + Sync {
    /// The SD header type used by this payload implementation.
    type SdHeader: WireFormat + Clone + core::fmt::Debug + Eq;

    /// Get the Message ID for the payload
    fn message_id(&self) -> MessageId;
    /// Get the payload as a service discovery header
    fn as_sd_header(&self) -> Option<&Self::SdHeader>;
    /// Construct a payload from raw bytes and a message ID.
    /// # Errors
    /// - If the message ID is not supported
    /// - If the payload bytes cannot be parsed
    fn from_payload_bytes(message_id: MessageId, payload: &[u8]) -> Result<Self, protocol::Error>;
    /// Create a `PayloadWireFormat` from a service discovery [Header](protocol::sd::Header)
    fn new_sd_payload(header: &Self::SdHeader) -> Self;
    /// Return the SD flags if this payload is a service discovery message.
    fn sd_flags(&self) -> Option<Flags>;
    /// Number of bytes required to write the payload
    fn required_size(&self) -> usize;
    /// Serialize the payload to a [Writer](embedded_io::Write)
    ///
    /// # Errors
    ///
    /// Returns an error if the payload cannot be written to the writer.
    fn encode<T: embedded_io::Write>(&self, writer: &mut T) -> Result<usize, protocol::Error>;

    /// Construct an SD header for subscribing to an event group.
    #[cfg(feature = "std")]
    #[allow(clippy::too_many_arguments)]
    fn new_subscription_sd_header(
        service_id: u16,
        instance_id: u16,
        major_version: u8,
        ttl: u32,
        event_group_id: u16,
        client_ip: std::net::Ipv4Addr,
        protocol: sd::TransportProtocol,
        client_port: u16,
    ) -> Self::SdHeader;

    /// Extract offered/stopped service endpoints from this SD payload.
    ///
    /// Default implementation returns an empty vec. Concrete implementations
    /// that have access to SD entries and options should override this.
    #[cfg(feature = "std")]
    fn offered_endpoints(&self) -> std::vec::Vec<OfferedEndpoint> {
        std::vec::Vec::new()
    }
}
