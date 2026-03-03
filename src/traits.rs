use crate::protocol::{self, MessageId, sd};

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

    fn from_payload_bytes(message_id: MessageId, payload: &[u8]) -> Result<Self, protocol::Error> {
        match message_id {
            MessageId::SD => {
                let sd_view = protocol::sd::SdHeaderView::parse(payload)?;
                Ok(Self {
                    header: sd_view.to_owned()?,
                })
            }
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
    fn from_payload_bytes_with_sd_message_id_succeeds() {
        let header = minimal_sd_header();
        let mut buf = [0u8; 64];
        let n = header.encode(&mut buf.as_mut_slice()).unwrap();
        let decoded = DiscoveryOnlyPayload::from_payload_bytes(MessageId::SD, &buf[..n]).unwrap();
        assert_eq!(decoded.as_sd_header().unwrap(), &header);
    }

    #[test]
    fn from_payload_bytes_with_non_sd_message_id_returns_error() {
        let non_sd_id = MessageId::new_from_service_and_method(0x1234, 0x0001);
        let err = DiscoveryOnlyPayload::<1, 1>::from_payload_bytes(non_sd_id, &[]).unwrap_err();
        assert!(matches!(err, protocol::Error::UnsupportedMessageID(_)));
    }

    #[test]
    fn encode_from_payload_bytes_round_trip() {
        let header: sd::Header<1, 0> = sd::Header::new_find_services(true, &[0x5B]);
        let payload = DiscoveryOnlyPayload::new_sd_payload(&header);
        let mut buf = [0u8; 28]; // required_size: 12 overhead + 16 entry = 28
        let n = payload.encode(&mut buf.as_mut_slice()).unwrap();
        assert_eq!(n, payload.required_size());
        let decoded = DiscoveryOnlyPayload::from_payload_bytes(MessageId::SD, &buf[..]).unwrap();
        assert_eq!(decoded, payload);
    }
}
