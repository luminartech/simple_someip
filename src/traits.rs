use crate::protocol::sd;
use crate::protocol::{self, MessageId, sd::Flags};

/// The service instance an SD offer or stop offer names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct OfferedInstance {
    /// The service instance.
    pub service: crate::ServiceInstance,
    /// The minor version of the service interface.
    pub minor_version: u32,
}

/// An offer or stop offer entry of an SD message.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ServiceOffer<'a> {
    /// The message's sender offers `instance` at the endpoints among
    /// `options`.
    Offer {
        /// The offered instance.
        instance: OfferedInstance,
        /// The options the offer entry references.
        options: sd::ReferencedOptions<core::slice::Iter<'a, sd::Options>>,
    },
    /// The message's sender no longer offers `instance`.
    Stop {
        /// The instance no longer offered.
        instance: OfferedInstance,
    },
}

/// Visits a [`ServiceOffer`] for every offer and stop offer in `header` that
/// names no wildcard.
#[cfg(any(feature = "std", feature = "bare_metal"))]
pub(crate) fn for_each_service_offer<'a>(
    header: sd::Header<'a>,
    mut f: impl FnMut(ServiceOffer<'a>),
) {
    use sd::Entry;
    for (entry, options) in header.entries_with_options() {
        let (Entry::OfferService(svc) | Entry::StopOfferService(svc)) = entry else {
            continue;
        };
        let Some(service) =
            crate::ServiceInstance::new(svc.service_id, svc.instance_id, svc.major_version)
        else {
            continue;
        };
        let instance = OfferedInstance {
            service,
            minor_version: svc.minor_version,
        };
        f(if matches!(entry, Entry::StopOfferService(_)) {
            ServiceOffer::Stop { instance }
        } else {
            ServiceOffer::Offer { instance, options }
        });
    }
}

/// Crate-local conveniences over any codec [`Encode`](automotive_wire_codec::Encode) type.
///
/// The codec's `Encode` trait provides `encode`, `encoded_size`, and
/// `encode_to_slice`; this extension adds the heap-allocating
/// `encode_to_vec` helper for `std` builds. It is blanket-implemented for
/// every `Encode` type, so bringing it into scope makes `encode_to_vec`
/// available anywhere.
pub trait EncodeExt: automotive_wire_codec::Encode {
    /// Encode into a newly allocated `Vec<u8>`.
    ///
    /// # Errors
    /// Returns an error if encoding fails.
    #[cfg(feature = "std")]
    fn encode_to_vec(&self) -> Result<std::vec::Vec<u8>, Self::Error> {
        let mut buf = std::vec![0u8; self.encoded_size()?];
        let mut cursor: &mut [u8] = &mut buf;
        self.encode(&mut cursor)?;
        Ok(buf)
    }
}

impl<T: automotive_wire_codec::Encode> EncodeExt for T {}

/// A trait for SOME/IP Payload types that can be serialized to a
/// [`Writer`](embedded_io::Write) and constructed from raw payload bytes.
///
/// The encode side is provided by the [`Encode`](automotive_wire_codec::Encode)
/// supertrait (`encoded_size` + `encode`); implementors get `encode_to_slice`
/// and — under `std` — the crate's [`EncodeExt`]`::encode_to_vec` for free.
///
/// Note that SOME/IP payloads are not self identifying, so the [Message ID](protocol::MessageId)
/// must be provided by the caller: `Encode` alone cannot reconstruct a payload
/// from bytes, which is why [`from_payload_bytes`](Self::from_payload_bytes)
/// remains an inherent requirement.
pub trait PayloadWireFormat:
    automotive_wire_codec::Encode<Error = protocol::Error> + core::fmt::Debug + Send + Sized + Sync
{
    /// The SD header type used by this payload implementation.
    // `Send + Sync` used to come for free from this trait's predecessor's own
    // `Send + Sync` supertrait (removed in the codec migration). The codec's
    // `Encode` has no such supertrait, but the client's channel-carried types
    // (`DiscoveryMessage`,
    // `ClientUpdate`, `ControlMessage`) embed `SdHeader` and flow through
    // `Send`-bounded channels, so the bound is pervasive rather than
    // localized. Restate it here on the associated type (all concrete
    // `SdHeader` types — `VecSdHeader`, `HeaplessSdHeader`, `sd::Header<'a>`,
    // and the test header — are plain owned/borrowed structs that are auto
    // `Send + Sync`), instead of threading a `where` clause through every
    // client type definition and impl.
    // `Error = protocol::Error` matches the bound this trait already puts on
    // `Self`. Without it the associated error type is opaque, which is what
    // pushed `Message::new_sd` into swallowing a failed `encoded_size` with
    // `unwrap_or(0)` -- it could not name the error to propagate it. Every
    // concrete `SdHeader` already uses `protocol::Error`, so this costs
    // nothing in tree and closes the hole for downstream impls.
    type SdHeader: automotive_wire_codec::Encode<Error = protocol::Error>
        + Clone
        + core::fmt::Debug
        + Eq
        + Send
        + Sync;

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

    /// Visit each offer and stop offer in this SD payload with `f`, as a
    /// [`ServiceOffer`]. An entry that names a wildcard is not a
    /// [`ServiceInstance`](crate::ServiceInstance) and is skipped.
    ///
    /// Visitor pattern (rather than returning a `Vec`) so the trait
    /// is `no_std`-compatible. The `Client` run loop uses this to
    /// auto-populate its service registry from inbound discovery messages.
    ///
    /// The default implementation visits nothing — payload types
    /// that don't carry SD entries (e.g. application payloads) leave
    /// it unimplemented; SD-bearing types (e.g. `RawPayload`'s
    /// `VecSdHeader` payload) override.
    fn for_each_service_offer<'a, F>(&'a self, _f: F)
    where
        F: FnMut(ServiceOffer<'a>),
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
    /// [`Self::for_each_service_offer`]; default visits nothing.
    fn for_each_service_instance<F>(&self, _f: F)
    where
        F: FnMut(u16, u16),
    {
    }

    /// Convenience accessor returning all offers and stop offers as a heap
    /// `Vec`. Wraps [`Self::for_each_service_offer`] so std users
    /// get the original ergonomic shape; bare-metal users use the
    /// visitor directly. Gated on `feature = "std"`.
    #[cfg(feature = "std")]
    fn service_offers(&self) -> std::vec::Vec<ServiceOffer<'_>> {
        let mut out = std::vec::Vec::new();
        self.for_each_service_offer(|offer| out.push(offer));
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
