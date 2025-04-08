use crate::protocol::{self, MessageId};

/// A trait for types that can be deserialized from a
/// [`Reader`](https://doc.rust-lang.org/std/io/trait.Read.html) and serialized
/// to a [`Writer`](https://doc.rust-lang.org/std/io/trait.Write.html).
///
/// `WireFormat` acts as the base trait for all types that can be serialized and deserialized
/// as part of the Simple SOME/IP ecosystem.
pub trait WireFormat: Send + Sized + Sync {
    /// Deserialize a value from a byte stream.
    /// Returns Ok(`Some(value)`) if the stream contains a complete value.
    /// Returns Ok(`None`) if the stream is empty.
    /// # Errors
    /// - if the stream is not in the expected format
    /// - if the stream contains partial data
    fn from_reader<T: std::io::Read>(reader: &mut T) -> Result<Self, protocol::Error>;

    /// Returns the number of bytes required to serialize this value.
    fn required_size(&self) -> usize;

    /// Serialize a value to a byte stream.
    /// Returns the number of bytes written.
    /// # Errors
    /// - If the data cannot be written to the stream
    fn to_writer<T: std::io::Write>(&self, writer: &mut T) -> Result<usize, protocol::Error>;
}

/// A trait for SOME/IP Payload types that can be deserialized from a
/// [`Reader`](std::io::Read) and serialized to a [`Writer`](std::io::Write).
/// Note that SOME/IP payloads are not self identifying, so the [Message ID](protocol::MessageId)
/// must be provided by the caller after reading from the [SOME/IP header](protocol::Header).
pub trait PayloadWireFormat: std::fmt::Debug + Send + Sized + Sync {
    /// Get the Message ID for te payload
    fn message_id(&self) -> MessageId;
    /// Get the payload as a service discovery header
    fn as_sd_header(&self) -> Option<&crate::protocol::sd::Header>;
    /// Deserialize a payload from a [Reader](std::io::Read) given the Message ID.
    fn from_reader_with_message_id<T: std::io::Read>(
        message_id: MessageId,
        reader: &mut T,
    ) -> Result<Self, protocol::Error>;
    /// Create a PayloadWireFormat from a service discovery [Header](protocol::sd::Header)
    fn new_sd_payload(header: &crate::protocol::sd::Header) -> Self;
    /// Number of bytes required to write the payload
    fn required_size(&self) -> usize;
    /// Serialize the payload to a [Writer](std::io::Write)
    fn to_writer<T: std::io::Write>(&self, writer: &mut T) -> Result<usize, protocol::Error>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryOnlyPayload {
    header: crate::protocol::sd::Header,
}

impl PayloadWireFormat for DiscoveryOnlyPayload {
    fn message_id(&self) -> MessageId {
        MessageId::SD
    }

    fn as_sd_header(&self) -> Option<&crate::protocol::sd::Header> {
        Some(&self.header)
    }

    fn from_reader_with_message_id<T: std::io::Read>(
        message_id: MessageId,
        reader: &mut T,
    ) -> Result<Self, protocol::Error> {
        if message_id.is_sd() {
            Ok(Self {
                header: protocol::sd::Header::from_reader(reader)?,
            })
        } else {
            Err(protocol::Error::UnsupportedMessageID(message_id))
        }
    }

    fn new_sd_payload(header: &crate::protocol::sd::Header) -> Self {
        Self {
            header: header.clone(),
        }
    }

    fn required_size(&self) -> usize {
        self.header.required_size()
    }

    fn to_writer<T: std::io::Write>(&self, writer: &mut T) -> Result<usize, protocol::Error> {
        self.header.to_writer(writer)
    }
}
