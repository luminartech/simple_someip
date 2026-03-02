use crate::protocol::{self, MessageId, sd};

/// A trait for types that can be deserialized from a
/// [`Reader`](embedded_io::Read) and serialized to a [`Writer`](embedded_io::Write).
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
    fn decode<T: embedded_io::Read>(reader: &mut T) -> Result<Self, protocol::Error>;

    /// Returns the number of bytes required to serialize this value.
    fn required_size(&self) -> usize;

    /// Serialize a value to a byte stream.
    /// Returns the number of bytes written.
    /// # Errors
    /// - If the data cannot be written to the stream
    fn encode<T: embedded_io::Write>(&self, writer: &mut T) -> Result<usize, protocol::Error>;
}

/// A trait for SOME/IP Payload types that can be deserialized from a
/// [`Reader`](embedded_io::Read) and serialized to a [`Writer`](embedded_io::Write).
/// Note that SOME/IP payloads are not self identifying, so the [Message ID](protocol::MessageId)
/// must be provided by the caller after reading from the [SOME/IP header](protocol::Header).
pub trait PayloadWireFormat: core::fmt::Debug + Send + Sized + Sync {
    /// The SD header type used by this payload implementation.
    type SdHeader: WireFormat + Clone + core::fmt::Debug + Eq;

    /// Get the Message ID for the payload
    fn message_id(&self) -> MessageId;
    /// Get the payload as a service discovery header
    fn as_sd_header(&self) -> Option<&Self::SdHeader>;
    /// Deserialize a payload from a [Reader](embedded_io::Read) given the Message ID.
    fn decode_with_message_id<T: embedded_io::Read>(
        message_id: MessageId,
        reader: &mut T,
    ) -> Result<Self, protocol::Error>;
    /// Create a `PayloadWireFormat` from a service discovery [Header](protocol::sd::Header)
    fn new_sd_payload(header: &Self::SdHeader) -> Self;
    /// Number of bytes required to write the payload
    fn required_size(&self) -> usize;
    /// Serialize the payload to a [Writer](embedded_io::Write)
    fn encode<T: embedded_io::Write>(&self, writer: &mut T) -> Result<usize, protocol::Error>;
}

/// A simple implementation of [`PayloadWireFormat`] that only supports SOME/IP-SD messages.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryOnlyPayload<
    const E: usize = { sd::MAX_SD_ENTRIES },
    const O: usize = { sd::MAX_SD_OPTIONS },
> {
    header: sd::Header<E, O>,
}

impl<const E: usize, const O: usize> PayloadWireFormat for DiscoveryOnlyPayload<E, O> {
    type SdHeader = sd::Header<E, O>;

    fn message_id(&self) -> MessageId {
        MessageId::SD
    }

    fn as_sd_header(&self) -> Option<&sd::Header<E, O>> {
        Some(&self.header)
    }

    fn decode_with_message_id<T: embedded_io::Read>(
        message_id: MessageId,
        reader: &mut T,
    ) -> Result<Self, protocol::Error> {
        match message_id {
            MessageId::SD => Ok(Self {
                header: sd::Header::decode(reader)?,
            }),

            _ => Err(protocol::Error::UnsupportedMessageID(message_id)),
        }
    }

    fn new_sd_payload(header: &sd::Header<E, O>) -> Self {
        Self {
            header: header.clone(),
        }
    }

    fn required_size(&self) -> usize {
        self.header.required_size()
    }

    fn encode<T: embedded_io::Write>(&self, writer: &mut T) -> Result<usize, protocol::Error> {
        self.header.encode(writer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{MessageId, sd};

    fn minimal_sd_header() -> sd::Header {
        sd::Header::new_find_services(false, &[])
    }

    #[test]
    fn message_id_is_always_sd() {
        let payload = DiscoveryOnlyPayload::new_sd_payload(&minimal_sd_header());
        assert_eq!(payload.message_id(), MessageId::SD);
    }

    #[test]
    fn as_sd_header_returns_some() {
        let header = minimal_sd_header();
        let payload = DiscoveryOnlyPayload::new_sd_payload(&header);
        assert_eq!(payload.as_sd_header(), Some(&header));
    }

    #[test]
    fn new_sd_payload_round_trips_header() {
        let header = minimal_sd_header();
        let payload = DiscoveryOnlyPayload::new_sd_payload(&header);
        assert_eq!(payload.as_sd_header().unwrap(), &header);
    }

    #[test]
    fn required_size_matches_header() {
        let header = minimal_sd_header();
        let payload = DiscoveryOnlyPayload::new_sd_payload(&header);
        assert_eq!(payload.required_size(), header.required_size());
    }

    #[test]
    fn decode_with_sd_message_id_succeeds() {
        let header = minimal_sd_header();
        let mut buf = [0u8; 64];
        let n = header.encode(&mut buf.as_mut_slice()).unwrap();
        let decoded =
            DiscoveryOnlyPayload::decode_with_message_id(MessageId::SD, &mut &buf[..n]).unwrap();
        assert_eq!(decoded.as_sd_header().unwrap(), &header);
    }

    #[test]
    fn decode_with_non_sd_message_id_returns_error() {
        let non_sd_id = MessageId::new_from_service_and_method(0x1234, 0x0001);
        let mut empty: &[u8] = &[];
        let err = DiscoveryOnlyPayload::<1, 1>::decode_with_message_id(non_sd_id, &mut empty)
            .unwrap_err();
        assert!(matches!(err, protocol::Error::UnsupportedMessageID(_)));
    }

    #[cfg(feature = "std")]
    #[test]
    fn encode_decode_round_trip() {
        let header = sd::Header::new_find_services(true, &[0x5B]);
        let payload = DiscoveryOnlyPayload::new_sd_payload(&header);
        let mut buf = std::vec::Vec::new();
        let n = payload.encode(&mut buf).unwrap();
        assert_eq!(n, payload.required_size());
        let decoded =
            DiscoveryOnlyPayload::decode_with_message_id(MessageId::SD, &mut buf.as_slice())
                .unwrap();
        assert_eq!(decoded, payload);
    }
}
