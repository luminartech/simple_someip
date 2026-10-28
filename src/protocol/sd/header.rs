use crate::protocol::byte_order::WriteBytesExt;

use automotive_wire_codec::{Decode, DecodeIter, DecodeIterator, Encode};

use super::{
    Entry, EntryView, Flags, OptionView, Options, ReferencedOptions,
    entry::{ENTRY_SIZE, EntryIter},
    options::OptionIter,
};

/// An SD header that borrows its entries and options slices.
///
/// Used for constructing and encoding outgoing SD messages. For zero-copy
/// parsing of incoming SD messages, see [`SdHeaderView`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Header<'a> {
    /// The SD flags byte (reboot + unicast).
    pub flags: Flags,
    /// The SD entries.
    pub entries: &'a [Entry],
    /// The SD options.
    pub options: &'a [Options],
}

impl<'a> Header<'a> {
    /// Creates a new SD header from the given flags, entries, and options.
    #[must_use]
    pub const fn new(flags: Flags, entries: &'a [Entry], options: &'a [Options]) -> Self {
        Self {
            flags,
            entries,
            options,
        }
    }

    fn entries_size(&self) -> Result<usize, crate::protocol::Error> {
        self.entries.iter().map(Entry::encoded_size).sum()
    }

    /// Each entry of this header with the options it references in this
    /// header.
    pub fn entries_with_options(
        self,
    ) -> impl Iterator<Item = (&'a Entry, ReferencedOptions<core::slice::Iter<'a, Options>>)> + 'a
    {
        let options = self.options;
        self.entries
            .iter()
            .map(move |entry| (entry, entry.option_runs().select(options.iter())))
    }

    fn options_size(&self) -> Result<usize, crate::protocol::Error> {
        self.options.iter().map(Options::encoded_size).sum()
    }
}

/// Zero-copy view into an SD header payload.
///
/// Created by [`SdHeaderView::parse`], which validates the SD header and its
/// options upfront. This makes the entry and option iterators infallible.
/// Entries of unknown type are skipped; options of unknown type keep their
/// place.
///
/// # Invariant
///
/// The type system carries no proof tying the cached `entry_count` /
/// `option_count` to `entries_buf` / `options_buf`. `parse` validates the
/// options once (draining their [`DecodeIterator`]) and caches both
/// counts; [`entries`](SdHeaderView::entries) and
/// [`options`](SdHeaderView::options) then re-slice the buffers without
/// re-running the option length/transport checks, trusting that walk, and
/// `entries` reads each type byte only to skip unknown types. Nothing but
/// `parse` constructs this type, so the trust holds.
#[derive(Clone, Copy, Debug)]
pub struct SdHeaderView<'a> {
    flags: Flags,
    entries_buf: &'a [u8],
    options_buf: &'a [u8],
    /// Number of entries of known type.
    entry_count: usize,
    /// Number of options, including those of unknown type.
    option_count: usize,
}

impl<'a> SdHeaderView<'a> {
    /// Parse and validate an SD header from `buf`.
    ///
    /// Validates:
    /// - Buffer has enough data for flags + `entries_size` + entries + `options_size` + options
    /// - `entries_size` is a multiple of `ENTRY_SIZE` (16)
    /// - Every option of a known type has a valid length, and IP-bearing options
    ///   have a recognized transport protocol byte
    ///
    /// An entry of unknown type is skipped and an option of unknown type keeps
    /// its place, so the message's other entries still apply and option
    /// indices still line up.
    ///
    /// # Errors
    ///
    /// Returns an error if the buffer is too short, `entries_size` is not a multiple of 16,
    /// or an option of a known type has an invalid length or transport protocol byte.
    pub fn parse(buf: &'a [u8]) -> Result<Self, crate::protocol::Error> {
        // `SdBody::decode` does the O(1) slicing and length checks (flags,
        // entries_size, options_size, section bounds). The entries are only
        // counted, skipping unknown types; the options are validated by the
        // walk below. The accessors trust both counts (see the type-level
        // invariant).
        let (body, _rest) = SdBody::decode(buf)?;

        let entry_count = EntryIter::new(body.entries_buf).len();

        // Validating walk over the options section. `OptionView`'s decode
        // only slices by the length field (surfacing truncation);
        // `validate()` checks a known type's length and transport-protocol
        // byte, and passes an unknown type.
        let mut option_count = 0usize;
        for option in body.options() {
            option?.validate()?;
            option_count += 1;
        }

        Ok(Self {
            flags: body.flags,
            entries_buf: body.entries_buf,
            options_buf: body.options_buf,
            entry_count,
            option_count,
        })
    }

    /// Returns the SD flags.
    #[must_use]
    pub fn flags(&self) -> Flags {
        self.flags
    }

    /// Returns an infallible iterator over the SD entries.
    ///
    /// Re-slices `entries_buf` at the fixed 16-byte stride, skipping entries
    /// of unknown type, so [`EntryView::entry_type`] of every yielded entry is
    /// `Ok`.
    /// The returned [`EntryIter`] is [`ExactSizeIterator`], its length being
    /// [`entry_count`](SdHeaderView::entry_count).
    #[must_use]
    pub fn entries(&self) -> EntryIter<'a> {
        EntryIter::with_known(self.entries_buf, self.entry_count)
    }

    /// Returns an infallible iterator over the SD options.
    ///
    /// Re-slices the already-validated `options_buf` by each option's length
    /// field; it never re-runs option validation (done once in
    /// [`parse`](SdHeaderView::parse)). An option of unknown type is included
    /// in its place.
    /// Options have no fixed stride, so [`OptionIter`] is not itself
    /// [`ExactSizeIterator`]; use [`option_count`](SdHeaderView::option_count)
    /// for the cached element count.
    #[must_use]
    pub fn options(&self) -> OptionIter<'a> {
        OptionIter::new(self.options_buf)
    }

    /// Each entry of known type with the options it references in this
    /// header.
    pub fn entries_with_options(
        &self,
    ) -> impl Iterator<Item = (EntryView<'a>, ReferencedOptions<OptionIter<'a>>)> + 'a {
        let options = self.options();
        self.entries()
            .map(move |entry| (entry, entry.option_runs().select(options.clone())))
    }

    /// Returns the number of entries of known type in this SD header.
    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.entry_count
    }

    /// Returns the number of options in this SD header, including those of
    /// unknown type.
    ///
    /// This is the count cached by the construction-time validating walk, the
    /// options' analogue of [`entry_count`](SdHeaderView::entry_count).
    #[must_use]
    pub fn option_count(&self) -> usize {
        self.option_count
    }
}

/// Lazy zero-copy view over an SD payload body.
///
/// [`SdBody::decode`] performs only the O(1) flag decode and section slicing
/// (with the accompanying length / `entries_size`-multiple checks); it does NOT
/// walk the entries validating their type bytes, nor the options validating
/// their type / length / transport-protocol bytes. The items yielded by the
/// lazy [`DecodeIter`] adapters returned by [`SdBody::entries`] /
/// [`SdBody::options`] check what each accessor reads.
///
/// Contrast with [`SdHeaderView`], which validates its options upfront and
/// skips entries of unknown type, so its iterators are infallible.
#[derive(Clone, Copy, Debug)]
pub struct SdBody<'a> {
    flags: Flags,
    entries_buf: &'a [u8],
    options_buf: &'a [u8],
}

impl<'a> SdBody<'a> {
    /// Returns the SD flags.
    #[must_use]
    pub fn flags(&self) -> Flags {
        self.flags
    }

    /// Returns a lazy iterator over the SD entries.
    ///
    /// Each item is a `Result<EntryView, Error>`; a malformed/truncated entry
    /// surfaces as an `Err`. The entry-type byte is not validated here — call
    /// [`EntryView::entry_type`] / [`EntryView::to_owned`] to validate it.
    #[must_use]
    pub fn entries(&self) -> DecodeIterator<'a, EntryView<'a>> {
        EntryView::iter(self.entries_buf)
    }

    /// Returns a lazy iterator over the SD options.
    ///
    /// Each item is a `Result<OptionView, Error>`; a malformed/truncated option
    /// surfaces as an `Err`. Option type / length / transport-protocol bytes
    /// are not validated here — validate them via the `OptionView` accessors.
    #[must_use]
    pub fn options(&self) -> DecodeIterator<'a, OptionView<'a>> {
        OptionView::iter(self.options_buf)
    }
}

impl<'a> Decode<'a> for SdBody<'a> {
    type Error = crate::protocol::Error;

    /// Decode and slice an SD payload body from the front of `buf`.
    ///
    /// Performs only the flag decode and the section slicing / length checks
    /// (buffer minimum, `entries_size` multiple-of-16, and section bounds). It
    /// deliberately does NOT validate entry-type bytes or option contents —
    /// see the type-level docs.
    ///
    /// # Errors
    ///
    /// Returns [`Incomplete`](automotive_wire_codec::Incomplete) if the buffer
    /// is too short for the declared sections, or
    /// [`IncorrectEntriesSize`](super::Error::IncorrectEntriesSize) if
    /// `entries_size` is not a multiple of `ENTRY_SIZE` (16).
    fn decode(buf: &'a [u8]) -> Result<(Self, &'a [u8]), Self::Error> {
        // Minimum: 4 (flags+reserved) + 4 (entries_size) + 4 (options_size) = 12
        if buf.len() < 12 {
            return Err(automotive_wire_codec::Incomplete {
                needed: 12,
                available: buf.len(),
            }
            .into());
        }

        let flags = Flags::from(buf[0]);
        // bytes [1..4] are reserved

        let entries_size = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;

        if !entries_size.is_multiple_of(ENTRY_SIZE) {
            return Err(super::Error::IncorrectEntriesSize(entries_size).into());
        }

        // All section-bound arithmetic is `checked_add`: on a 32-bit `usize`
        // (`no_std` embedded targets) a hostile `entries_size` / `options_size`
        // near `u32::MAX` would otherwise wrap `8 + entries_size + 4` or
        // `options_start + options_size` and pass the length check with a bogus
        // small bound. An overflow means the buffer cannot possibly hold the
        // declared sections, so it is reported as `Incomplete`.
        let overflow = || automotive_wire_codec::Incomplete {
            needed: usize::MAX,
            available: buf.len(),
        };

        // Need entries data + 4 bytes for options_size field.
        let entries_end = 8usize.checked_add(entries_size).ok_or_else(overflow)?;
        let options_size_offset = entries_end;
        let entries_section_end = entries_end.checked_add(4).ok_or_else(overflow)?;
        if buf.len() < entries_section_end {
            return Err(automotive_wire_codec::Incomplete {
                needed: entries_section_end,
                available: buf.len(),
            }
            .into());
        }

        let entries_buf = &buf[8..options_size_offset];

        let options_size = u32::from_be_bytes([
            buf[options_size_offset],
            buf[options_size_offset + 1],
            buf[options_size_offset + 2],
            buf[options_size_offset + 3],
        ]) as usize;

        let options_start = entries_section_end;
        let options_end = options_start
            .checked_add(options_size)
            .ok_or_else(overflow)?;
        if buf.len() < options_end {
            return Err(automotive_wire_codec::Incomplete {
                needed: options_end,
                available: buf.len(),
            }
            .into());
        }

        let options_buf = &buf[options_start..options_end];
        let rest = &buf[options_end..];

        Ok((
            Self {
                flags,
                entries_buf,
                options_buf,
            },
            rest,
        ))
    }
}

impl Encode for Header<'_> {
    type Error = crate::protocol::Error;

    /// Returns this header's wire size.
    ///
    /// # Errors
    ///
    /// Returns [`UnencodableOption`](super::Error::UnencodableOption) when
    /// the header holds a received option of unknown type,
    /// [`Options::Unknown`], or [`InvalidTtl`](super::Error::InvalidTtl) when
    /// an entry's TTL is out of range.
    fn encoded_size(&self) -> Result<usize, Self::Error> {
        Ok(12 + self.entries_size()? + self.options_size()?)
    }

    /// Writes this header to `writer`.
    ///
    /// # Errors
    ///
    /// Returns the [`encoded_size`](Encode::encoded_size) error before writing
    /// anything, or the writer's error.
    fn encode(&self, writer: &mut impl embedded_io::Write) -> Result<usize, Self::Error> {
        let entries_size = self.entries_size()?;
        let options_size = self.options_size()?;
        writer.write_u8(u8::from(self.flags))?;
        let reserved: [u8; 3] = [0; 3];
        writer.write_bytes(&reserved)?;
        writer.write_u32_be(u32::try_from(entries_size).expect("entries size fits u32"))?;
        for entry in self.entries {
            entry.encode(writer)?;
        }
        writer.write_u32_be(u32::try_from(options_size).expect("options size fits u32"))?;
        for option in self.options {
            option.encode(writer)?;
        }
        Ok(12 + entries_size + options_size)
    }
}

#[cfg(test)]
mod tests {
    use core::net::Ipv4Addr;

    use super::*;
    use crate::protocol::sd::{
        Error as SdError, EventGroupEntry, OptionType, OptionsCount, RebootFlag, ServiceEntry,
        TransportProtocol,
        options::{
            IPV4_OPTION_IP_OFFSET, IPV4_OPTION_LENGTH_FIELD, IPV4_OPTION_PORT_OFFSET,
            IPV4_OPTION_PROTOCOL_OFFSET, IPV4_OPTION_WIRE_SIZE,
        },
    };
    use automotive_wire_codec::Encode;

    fn ipv4_endpoint_bytes(ip: [u8; 4], protocol: u8, port: u16) -> [u8; IPV4_OPTION_WIRE_SIZE] {
        let mut b = [0u8; IPV4_OPTION_WIRE_SIZE];
        b[0..2].copy_from_slice(&IPV4_OPTION_LENGTH_FIELD.to_be_bytes());
        b[2] = u8::from(OptionType::IpV4Endpoint);
        // b[3] is the discard flag (0).
        b[IPV4_OPTION_IP_OFFSET..IPV4_OPTION_IP_OFFSET + 4].copy_from_slice(&ip);
        // b[IPV4_OPTION_IP_OFFSET + 4] is reserved (0).
        b[IPV4_OPTION_PROTOCOL_OFFSET] = protocol;
        b[IPV4_OPTION_PORT_OFFSET..IPV4_OPTION_PORT_OFFSET + 2]
            .copy_from_slice(&port.to_be_bytes());
        b
    }

    fn raw_header(entries_size: u32, options_size: u32) -> [u8; 12] {
        let mut b = [0u8; 12];
        // flags = 0, reserved = 0
        b[4..8].copy_from_slice(&entries_size.to_be_bytes());
        b[8..12].copy_from_slice(&options_size.to_be_bytes());
        b
    }

    #[test]
    fn header_new_stores_fields() {
        let flags = Flags::new_sd(RebootFlag::RecentlyRebooted);
        let entries: &[Entry] = &[];
        let options: &[Options] = &[];
        let h = Header::new(flags, entries, options);
        assert_eq!(h.flags, flags);
        assert!(h.entries.is_empty());
        assert!(h.options.is_empty());
    }

    #[test]
    fn service_offer_round_trips() {
        let ip = Ipv4Addr::new(192, 168, 1, 10);
        let entry = Entry::OfferService(ServiceEntry {
            service_id: 0x1234,
            instance_id: 0x0001,
            major_version: 1,
            ttl: 0xFF_FFFF,
            index_first_options_run: 0,
            index_second_options_run: 0,
            options_count: OptionsCount::new(1, 0),
            minor_version: 0,
        });
        let endpoint = Options::IpV4Endpoint {
            ip,
            protocol: TransportProtocol::Udp,
            port: 30509,
        };
        let entries = [entry];
        let options = [endpoint];
        let h = Header::new(
            Flags::new_sd(RebootFlag::RecentlyRebooted),
            &entries,
            &options,
        );
        assert_eq!(h.encoded_size().unwrap(), 40);
        let mut buf = [0u8; 64];
        h.encode(&mut buf.as_mut_slice()).unwrap();
        let view = SdHeaderView::parse(&buf[..h.encoded_size().unwrap()]).unwrap();
        assert_eq!(view.entry_count(), 1);
        let entry_view = view.entries().next().unwrap();
        assert_eq!(entry_view.service_id(), 0x1234);
    }

    #[test]
    fn subscribe_ack_round_trips() {
        let entry = Entry::SubscribeAckEventGroup(EventGroupEntry::new(
            0xAAAA, 0x0001, 1, 0xFF_FFFF, 0x0010,
        ));
        let entries = [entry];
        let h = Header::new(Flags::new_sd(RebootFlag::RecentlyRebooted), &entries, &[]);
        assert_eq!(h.encoded_size().unwrap(), 28);
        let mut buf = [0u8; 32];
        h.encode(&mut buf.as_mut_slice()).unwrap();
        let view = SdHeaderView::parse(&buf[..h.encoded_size().unwrap()]).unwrap();
        assert_eq!(view.entry_count(), 1);
    }

    #[test]
    fn parse_exact_size_slice_succeeds() {
        let entry = Entry::OfferService(ServiceEntry {
            service_id: 0x1234,
            instance_id: 0x0001,
            major_version: 1,
            ttl: 0xFF_FFFF,
            index_first_options_run: 0,
            index_second_options_run: 0,
            options_count: OptionsCount::new(1, 0),
            minor_version: 0,
        });
        let endpoint = Options::IpV4Endpoint {
            ip: Ipv4Addr::new(192, 168, 1, 10),
            protocol: TransportProtocol::Udp,
            port: 30509,
        };
        let entries = [entry];
        let options = [endpoint];
        let h = Header::new(
            Flags::new_sd(RebootFlag::RecentlyRebooted),
            &entries,
            &options,
        );
        let mut buf = [0u8; 64];
        let n = h.encode(&mut buf.as_mut_slice()).unwrap();
        let view = SdHeaderView::parse(&buf[..n]).unwrap();
        assert_eq!(view.entry_count(), 1);
    }

    #[test]
    fn parse_options_size_below_minimum_returns_error() {
        // `parse` drains the option `DecodeIterator`, so a truncated options
        // section surfaces `Incomplete` with the needed / available byte counts.
        let prefix = raw_header(0, 2);
        let mut buf = [0u8; 14];
        buf[..12].copy_from_slice(&prefix);
        assert!(matches!(
            SdHeaderView::parse(&buf),
            Err(crate::protocol::Error::Incomplete(
                automotive_wire_codec::Incomplete {
                    needed: 4,
                    available: 2,
                }
            ))
        ));
    }

    #[test]
    fn parse_option_size_exceeds_declared_remaining_returns_error() {
        let prefix = raw_header(0, 5);
        let option = ipv4_endpoint_bytes([127, 0, 0, 1], 0x11, 1234);
        let mut buf = [0u8; 24];
        buf[..12].copy_from_slice(&prefix);
        buf[12..24].copy_from_slice(&option);
        assert!(matches!(
            SdHeaderView::parse(&buf),
            Err(crate::protocol::Error::Incomplete(
                automotive_wire_codec::Incomplete {
                    needed: 12,
                    available: 5,
                }
            ))
        ));
    }

    // --- SdHeaderView accessors ---

    #[test]
    fn sd_header_view_entry_count() {
        let entries = [
            Entry::FindService(ServiceEntry::find(0x0001)),
            Entry::FindService(ServiceEntry::find(0x0002)),
        ];
        let h = Header::new(Flags::new_sd(RebootFlag::RecentlyRebooted), &entries, &[]);
        let mut buf = [0u8; 64];
        h.encode(&mut buf.as_mut_slice()).unwrap();
        let view = SdHeaderView::parse(&buf[..h.encoded_size().unwrap()]).unwrap();
        assert_eq!(view.entry_count(), 2);
    }

    #[test]
    fn sd_header_view_accessors_yield_cached_counts() {
        // After a successful parse, the infallible accessors yield exactly the
        // cached counts and never panic.
        let ip = Ipv4Addr::new(192, 168, 1, 10);
        let entries = [
            Entry::FindService(ServiceEntry::find(0x0001)),
            Entry::FindService(ServiceEntry::find(0x0002)),
        ];
        let options = [
            Options::IpV4Endpoint {
                ip,
                protocol: TransportProtocol::Udp,
                port: 30509,
            },
            Options::IpV4Endpoint {
                ip,
                protocol: TransportProtocol::Tcp,
                port: 30510,
            },
        ];
        let h = Header::new(
            Flags::new_sd(RebootFlag::RecentlyRebooted),
            &entries,
            &options,
        );
        let mut buf = [0u8; 128];
        let n = h.encode(&mut buf.as_mut_slice()).unwrap();
        let view = SdHeaderView::parse(&buf[..n]).unwrap();
        assert_eq!(view.entry_count(), 2);
        assert_eq!(view.option_count(), 2);
        // Infallible accessors walk without panicking and match the counts.
        assert_eq!(view.entries().count(), view.entry_count());
        assert_eq!(view.options().count(), view.option_count());
        // EntryIter is ExactSizeIterator: its len matches the cached count.
        assert_eq!(view.entries().len(), view.entry_count());
    }

    #[test]
    fn parse_rejects_trailing_partial_option() {
        // options_size declares 12 bytes, but the single option's length field
        // claims a wire size of 16 (length = 13). The eager walk must reject
        // this at parse rather than silently truncating at accessor time.
        let prefix = raw_header(0, 12);
        let mut option = ipv4_endpoint_bytes([10, 0, 0, 1], 0x11, 30490);
        // Overwrite the length field to claim more bytes than are present.
        option[0..2].copy_from_slice(&13u16.to_be_bytes());
        let mut buf = [0u8; 24];
        buf[..12].copy_from_slice(&prefix);
        buf[12..24].copy_from_slice(&option);
        assert!(matches!(
            SdHeaderView::parse(&buf),
            Err(crate::protocol::Error::Incomplete(
                automotive_wire_codec::Incomplete {
                    needed: 16,
                    available: 12,
                }
            ))
        ));
    }

    #[test]
    fn sd_header_view_flags() {
        let h = Header::new(Flags::new_sd(RebootFlag::RecentlyRebooted), &[], &[]);
        let mut buf = [0u8; 16];
        h.encode(&mut buf.as_mut_slice()).unwrap();
        let view = SdHeaderView::parse(&buf[..h.encoded_size().unwrap()]).unwrap();
        assert_eq!(view.flags(), h.flags);
    }

    #[test]
    fn parse_incorrect_entries_size_returns_error() {
        let mut buf = [0u8; 12];
        buf[4..8].copy_from_slice(&5u32.to_be_bytes());
        assert!(matches!(
            SdHeaderView::parse(&buf),
            Err(crate::protocol::Error::Sd(SdError::IncorrectEntriesSize(5)))
        ));
    }

    #[test]
    fn parse_rejects_ipv4_option_with_invalid_transport_protocol() {
        // An IPv4 endpoint option with an otherwise-valid wire layout but a
        // transport protocol byte that is neither UDP (0x11) nor TCP (0x06)
        // must be rejected by parse, so downstream `as_ipv4()` calls cannot
        // observe a bad protocol byte at walk time.
        const SD_HEADER_PREFIX_SIZE: usize = 12;
        let options_size = u32::try_from(IPV4_OPTION_WIRE_SIZE).expect("wire size fits u32");
        let prefix = raw_header(0, options_size);
        let option = ipv4_endpoint_bytes([10, 0, 0, 1], 0xAB, 30490);
        let mut buf = [0u8; SD_HEADER_PREFIX_SIZE + IPV4_OPTION_WIRE_SIZE];
        buf[..SD_HEADER_PREFIX_SIZE].copy_from_slice(&prefix);
        buf[SD_HEADER_PREFIX_SIZE..].copy_from_slice(&option);
        assert!(matches!(
            SdHeaderView::parse(&buf),
            Err(crate::protocol::Error::Sd(
                SdError::InvalidOptionTransportProtocol(0xAB)
            ))
        ));
    }

    #[test]
    fn parse_keeps_an_entry_and_an_option_of_unknown_type() {
        use crate::protocol::sd::test_support::{
            UNKNOWN_OPTION_TYPE, endpoint_after_unknown_option, offer_after_unknown_entry,
            sd_body_with_unknown_entry_and_option,
        };
        let body = sd_body_with_unknown_entry_and_option();
        let view = SdHeaderView::parse(&body).expect("unknown types do not reject the message");

        let entries: heapless::Vec<_, 2> = view.entries().map(|e| e.to_owned().unwrap()).collect();
        assert_eq!(entries, [offer_after_unknown_entry()]);
        assert_eq!(view.entry_count(), 1);
        assert_eq!(view.entries().len(), 1);

        let options: heapless::Vec<_, 2> = view.options().collect();
        assert!(matches!(
            options[0].option_type(),
            Err(SdError::InvalidOptionType(UNKNOWN_OPTION_TYPE))
        ));
        assert_eq!(
            options[1].to_owned().unwrap(),
            endpoint_after_unknown_option()
        );
    }

    // --- SdBody (lazy decode) ---

    #[test]
    fn sd_body_decode_slices_sections() {
        let ip = Ipv4Addr::new(192, 168, 1, 10);
        let entry = Entry::OfferService(ServiceEntry {
            service_id: 0x1234,
            instance_id: 0x0001,
            major_version: 1,
            ttl: 0xFF_FFFF,
            index_first_options_run: 0,
            index_second_options_run: 0,
            options_count: OptionsCount::new(1, 0),
            minor_version: 0,
        });
        let endpoint = Options::IpV4Endpoint {
            ip,
            protocol: TransportProtocol::Udp,
            port: 30509,
        };
        let entries = [entry];
        let options = [endpoint];
        let h = Header::new(
            Flags::new_sd(RebootFlag::RecentlyRebooted),
            &entries,
            &options,
        );
        let mut buf = [0u8; 64];
        let n = h.encode(&mut buf.as_mut_slice()).unwrap();
        let (body, rest) = SdBody::decode(&buf[..n]).unwrap();
        assert!(rest.is_empty());
        assert_eq!(body.flags(), h.flags);
        // Lazy iterators recover the entry and option.
        let entry_view = body.entries().next().unwrap().unwrap();
        assert_eq!(entry_view.service_id(), 0x1234);
        let opt_view = body.options().next().unwrap().unwrap();
        assert_eq!(opt_view.as_ipv4().unwrap().0, ip);
    }

    #[test]
    fn sd_body_decode_returns_trailing_remainder() {
        let h = Header::new(Flags::new_sd(RebootFlag::RecentlyRebooted), &[], &[]);
        let mut buf = [0u8; 32];
        let n = h.encode(&mut buf.as_mut_slice()).unwrap();
        // Append 3 extra trailing bytes past the SD body.
        buf[n] = 0xDE;
        buf[n + 1] = 0xAD;
        buf[n + 2] = 0xBE;
        let (_body, rest) = SdBody::decode(&buf[..n + 3]).unwrap();
        assert_eq!(rest, &[0xDE, 0xAD, 0xBE]);
    }

    #[test]
    fn sd_body_decode_defers_entry_type_validation() {
        // A body whose single entry has an invalid entry-type byte (0x03)
        // must still decode successfully — SdBody does NOT walk entry types.
        // entries_size = 16 (valid multiple), options_size = 0.
        let mut buf = [0u8; 28];
        buf[4..8].copy_from_slice(&16u32.to_be_bytes());
        buf[8] = 0x03; // invalid entry type byte
        // bytes 24..28 = options_size = 0
        let (body, rest) = SdBody::decode(&buf).unwrap();
        assert!(rest.is_empty());
        // The lazy iterator produces the view; validation only fails on to_owned.
        let entry_view = body.entries().next().unwrap().unwrap();
        assert!(matches!(
            entry_view.to_owned(),
            Err(SdError::InvalidEntryType(0x03))
        ));
        // SdHeaderView::parse accepts the body and skips the entry.
        let view = SdHeaderView::parse(&buf).unwrap();
        assert_eq!(view.entry_count(), 0);
        assert_eq!(view.entries().count(), 0);
    }

    #[test]
    fn sd_body_decode_defers_option_validation() {
        // options_size = 12 with an IPv4 option carrying an invalid transport
        // protocol byte. SdBody slices it without complaint.
        const PREFIX: usize = 12;
        let options_size = u32::try_from(IPV4_OPTION_WIRE_SIZE).unwrap();
        let prefix = raw_header(0, options_size);
        let option = ipv4_endpoint_bytes([10, 0, 0, 1], 0xAB, 30490);
        let mut buf = [0u8; PREFIX + IPV4_OPTION_WIRE_SIZE];
        buf[..PREFIX].copy_from_slice(&prefix);
        buf[PREFIX..].copy_from_slice(&option);
        let (body, rest) = SdBody::decode(&buf).unwrap();
        assert!(rest.is_empty());
        let opt_view = body.options().next().unwrap().unwrap();
        assert!(matches!(
            opt_view.as_ipv4(),
            Err(SdError::InvalidOptionTransportProtocol(0xAB))
        ));
    }

    #[test]
    fn sd_body_decode_too_short_is_incomplete() {
        let buf = [0u8; 8];
        assert!(matches!(
            SdBody::decode(&buf),
            Err(crate::protocol::Error::Incomplete(
                automotive_wire_codec::Incomplete {
                    needed: 12,
                    available: 8,
                }
            ))
        ));
    }

    #[test]
    fn sd_body_decode_rejects_non_multiple_entries_size() {
        let mut buf = [0u8; 12];
        buf[4..8].copy_from_slice(&5u32.to_be_bytes());
        assert!(matches!(
            SdBody::decode(&buf),
            Err(crate::protocol::Error::Sd(SdError::IncorrectEntriesSize(5)))
        ));
    }

    #[test]
    fn sd_body_entries_remaining_len_reports_count() {
        let entries = [
            Entry::FindService(ServiceEntry::find(0x0001)),
            Entry::FindService(ServiceEntry::find(0x0002)),
        ];
        let h = Header::new(Flags::new_sd(RebootFlag::RecentlyRebooted), &entries, &[]);
        let mut buf = [0u8; 64];
        let n = h.encode(&mut buf.as_mut_slice()).unwrap();
        let (body, _rest) = SdBody::decode(&buf[..n]).unwrap();
        assert_eq!(body.entries().remaining_len(), Some(2));
    }

    #[test]
    fn encode_writes_nothing_when_an_option_cannot_be_encoded() {
        use crate::protocol::sd::test_support::{
            assert_same_error, sd_body_with_unknown_entry_and_option,
        };
        let body = sd_body_with_unknown_entry_and_option();
        let view = SdHeaderView::parse(&body).unwrap();
        let entries: heapless::Vec<_, 2> = view.entries().map(|e| e.to_owned().unwrap()).collect();
        let options: heapless::Vec<_, 2> = view.options().map(|o| o.to_owned().unwrap()).collect();
        let header = Header::new(view.flags(), &entries, &options);

        let mut buf = [0u8; 128];
        let mut writer = buf.as_mut_slice();
        let err = header.encode(&mut writer).unwrap_err();
        let option_err = options[0].encoded_size().unwrap_err();
        assert_same_error(&err, &option_err);
        assert_eq!(writer.len(), 128, "nothing was written");
    }

    #[test]
    fn an_entry_with_an_invalid_ttl_fails_sizing_and_writes_nothing() {
        let find = Entry::FindService(ServiceEntry::find(0x1234));
        let offer = Entry::OfferService(ServiceEntry {
            ttl: 0,
            ..ServiceEntry::find(0x1234)
        });
        let entries = [find, offer];
        let header = Header::new(Flags::new_sd(RebootFlag::RecentlyRebooted), &entries, &[]);

        assert!(matches!(
            header.encoded_size(),
            Err(crate::protocol::Error::Sd(SdError::InvalidTtl(0)))
        ));
        let mut buf = [0u8; 64];
        let mut writer = buf.as_mut_slice();
        assert!(matches!(
            header.encode(&mut writer),
            Err(crate::protocol::Error::Sd(SdError::InvalidTtl(0)))
        ));
        assert_eq!(writer.len(), 64, "nothing was written");
    }

    // --- entries_with_options ---

    fn endpoint_at(port: u16) -> Options {
        Options::IpV4Endpoint {
            ip: Ipv4Addr::new(192, 0, 2, 1),
            protocol: TransportProtocol::Udp,
            port,
        }
    }

    fn offer_with_runs(first: (u8, u8), second: (u8, u8)) -> Entry {
        Entry::OfferService(ServiceEntry {
            index_first_options_run: first.0,
            index_second_options_run: second.0,
            options_count: OptionsCount::new(first.1, second.1),
            ttl: 3,
            ..ServiceEntry::find(0x1234)
        })
    }

    /// Entries whose runs pick: the second option; the first and third; none;
    /// a run cut short by the end of the options; a run past the end; and an
    /// eventgroup entry's second option.
    fn entries_and_options() -> ([Entry; 6], [Options; 3]) {
        let subscribe = Entry::SubscribeEventGroup(EventGroupEntry {
            index_first_options_run: 1,
            options_count: OptionsCount::new(1, 0),
            ..EventGroupEntry::new(0x1234, 1, 1, 3, 0x0001)
        });
        (
            [
                offer_with_runs((1, 1), (0, 0)),
                offer_with_runs((0, 1), (2, 1)),
                offer_with_runs((0, 0), (0, 0)),
                offer_with_runs((2, 2), (0, 0)),
                offer_with_runs((3, 1), (0, 0)),
                subscribe,
            ],
            [endpoint_at(30509), endpoint_at(30600), endpoint_at(30700)],
        )
    }

    const EXPECTED_PORTS: [&[u16]; 6] = [&[30600], &[30509, 30700], &[], &[30700], &[], &[30600]];

    #[test]
    fn entries_with_options_pairs_each_entry_with_the_options_it_references() {
        let (entries, options) = entries_and_options();
        let header = Header::new(Flags::new_sd(RebootFlag::Continuous), &entries, &options);
        let paired: heapless::Vec<(&Entry, heapless::Vec<u16, 3>), 6> = header
            .entries_with_options()
            .map(|(entry, referenced)| {
                let ports = referenced
                    .map(|option| match option {
                        Options::IpV4Endpoint { port, .. } => *port,
                        other => panic!("unexpected option {other:?}"),
                    })
                    .collect();
                (entry, ports)
            })
            .collect();
        assert_eq!(paired.len(), entries.len());
        for ((entry, ports), (expected_entry, expected_ports)) in
            paired.iter().zip(entries.iter().zip(EXPECTED_PORTS))
        {
            assert_eq!(*entry, expected_entry);
            assert_eq!(ports.as_slice(), expected_ports, "entry {entry:?}");
        }
    }

    #[test]
    fn view_entries_with_options_pairs_each_entry_with_the_options_it_references() {
        let (entries, options) = entries_and_options();
        let header = Header::new(Flags::new_sd(RebootFlag::Continuous), &entries, &options);
        let mut buf = [0u8; 256];
        header.encode(&mut buf.as_mut_slice()).unwrap();
        let view = SdHeaderView::parse(&buf[..header.encoded_size().unwrap()]).unwrap();
        let ports: heapless::Vec<heapless::Vec<u16, 3>, 6> = view
            .entries_with_options()
            .map(|(_, referenced)| {
                referenced
                    .map(|option| option.as_ipv4().unwrap().2)
                    .collect()
            })
            .collect();
        assert_eq!(ports.len(), EXPECTED_PORTS.len());
        for (ports, expected) in ports.iter().zip(EXPECTED_PORTS) {
            assert_eq!(ports.as_slice(), expected);
        }
    }
}
