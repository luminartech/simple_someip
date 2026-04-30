use crate::protocol::sd;
use crate::protocol::{self, MessageId, sd::Flags};

/// Information about a service endpoint extracted from an SD message.
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
    pub addr: Option<core::net::SocketAddrV4>,
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

    /// Encode into a byte slice, returning the number of bytes written.
    ///
    /// # Errors
    /// Returns an error if `buf` is too small (requires at least
    /// [`required_size()`](Self::required_size) bytes).
    fn encode_to_slice(&self, buf: &mut [u8]) -> Result<usize, protocol::Error> {
        self.encode(&mut &mut *buf)
    }

    /// Encode into a newly allocated `Vec<u8>`.
    ///
    /// # Errors
    /// Returns an error if encoding fails.
    #[cfg(feature = "std")]
    fn encode_to_vec(&self) -> Result<std::vec::Vec<u8>, protocol::Error> {
        let mut buf = std::vec![0u8; self.required_size()];
        self.encode_to_slice(&mut buf)?;
        Ok(buf)
    }
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
    #[allow(clippy::too_many_arguments)]
    fn new_subscription_sd_header(
        service_id: u16,
        instance_id: u16,
        major_version: u8,
        ttl: u32,
        event_group_id: u16,
        client_ip: core::net::Ipv4Addr,
        protocol: sd::TransportProtocol,
        client_port: u16,
        reboot_flag: sd::RebootFlag,
    ) -> Self::SdHeader;

    /// Override the reboot flag on an SD header in-place.
    ///
    /// Used by `Client::sd_announcements_loop` to refresh the reboot
    /// flag per-tick from the client's tracked state. Defaults to a
    /// no-op so payload types that never participate in SD reboot
    /// tracking (e.g. `RawPayload` for static-only SD use) don't have
    /// to provide an impl that will never be called.
    fn set_reboot_flag(_header: &mut Self::SdHeader, _reboot: sd::RebootFlag) {}

    /// Visit each offered / stopped service endpoint in this SD
    /// payload with `f`.
    ///
    /// Visitor pattern (rather than returning a `Vec`) so the trait
    /// is `no_std`-compatible: the implementation walks its internal
    /// SD entries and invokes `f` for each `OfferedEndpoint`. The
    /// `Client` run loop uses this to auto-populate its service
    /// registry from inbound discovery messages.
    ///
    /// The default implementation visits nothing — payload types
    /// that don't carry SD entries (e.g. application payloads) leave
    /// it unimplemented; SD-bearing types (e.g. `RawPayload`'s
    /// `VecSdHeader` payload) override.
    fn for_each_offered_endpoint<F>(&self, _f: F)
    where
        F: FnMut(OfferedEndpoint),
    {
    }

    /// Visit `(service_id, instance_id)` for every SD entry in this
    /// payload, regardless of entry type, with `f`.
    ///
    /// Used by the `Client` run loop for per-service-instance
    /// session/reboot tracking so that all SD traffic (not just
    /// offers) contributes to reboot detection.
    ///
    /// Visitor pattern for the same `no_std` reason as
    /// [`Self::for_each_offered_endpoint`]; default visits nothing.
    fn for_each_service_instance<F>(&self, _f: F)
    where
        F: FnMut(u16, u16),
    {
    }

    /// Convenience accessor returning all offered endpoints as a heap
    /// `Vec`. Wraps [`Self::for_each_offered_endpoint`] so std users
    /// get the original ergonomic shape; bare-metal users use the
    /// visitor directly. Gated on `feature = "std"`.
    #[cfg(feature = "std")]
    fn offered_endpoints(&self) -> std::vec::Vec<OfferedEndpoint> {
        let mut out = std::vec::Vec::new();
        self.for_each_offered_endpoint(|ep| out.push(ep));
        out
    }

    /// Convenience accessor returning all `(service_id, instance_id)`
    /// pairs as a heap `Vec`. Wraps
    /// [`Self::for_each_service_instance`] for std users. Gated on
    /// `feature = "std"`.
    #[cfg(feature = "std")]
    fn service_instances(&self) -> std::vec::Vec<(u16, u16)> {
        let mut out = std::vec::Vec::new();
        self.for_each_service_instance(|svc, inst| out.push((svc, inst)));
        out
    }
}
