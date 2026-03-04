use core::net::Ipv4Addr;

use crate::protocol::byte_order::WriteBytesExt;

use crate::traits::WireFormat;

use super::{
    Entry, EventGroupEntry, Flags, Options, ServiceEntry, TransportProtocol,
    entry::{ENTRY_SIZE, EntryIter, EntryType, OptionsCount},
    options::{OptionIter, validate_option},
};

/// Default maximum number of SD entries in a single header.
pub const MAX_SD_ENTRIES: usize = 1;
/// Default maximum number of SD options in a single header.
pub const MAX_SD_OPTIONS: usize = 1;

/// Fixed-capacity vector of SD entries.
pub type SdEntries<const N: usize = MAX_SD_ENTRIES> = heapless::Vec<Entry, N>;
/// Fixed-capacity vector of SD options.
pub type SdOptions<const N: usize = MAX_SD_OPTIONS> = heapless::Vec<Options, N>;

/// An owned SOME/IP-SD header containing flags, entries, and options.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Header<
    const MAX_ENTRIES: usize = MAX_SD_ENTRIES,
    const MAX_OPTIONS: usize = MAX_SD_OPTIONS,
> {
    /// The SD flags byte (reboot + unicast).
    pub flags: Flags,
    /// The SD entries.
    pub entries: SdEntries<MAX_ENTRIES>,
    /// The SD options.
    pub options: SdOptions<MAX_OPTIONS>,
}

impl<const E: usize, const O: usize> Header<E, O> {
    /// Creates a new SD header from the given flags, entries, and options.
    #[must_use]
    pub fn new(flags: Flags, entries: SdEntries<E>, options: SdOptions<O>) -> Self {
        Self {
            flags,
            entries,
            options,
        }
    }

    /// Creates an SD header for offering a service with an IPv4 endpoint option.
    ///
    /// # Panics
    ///
    /// Panics if the const generic `E` or `O` is zero (cannot hold a single entry/option).
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new_service_offer(
        service_id: u16,
        instance_id: u16,
        major_version: u8,
        minor_version: u32,
        ttl: u32,
        client_ip: Ipv4Addr,
        protocol: TransportProtocol,
        client_port: u16,
    ) -> Self {
        let entry = Entry::OfferService(ServiceEntry {
            service_id,
            instance_id,
            major_version,
            ttl,
            index_first_options_run: 0,
            index_second_options_run: 0,
            options_count: OptionsCount::new(1, 0),
            minor_version,
        });
        let endpoint = Options::IpV4Endpoint {
            ip: client_ip,
            protocol,
            port: client_port,
        };
        let mut entries = SdEntries::new();
        let mut options = SdOptions::new();
        entries
            .push(entry)
            .expect("single SD entry exceeds capacity");
        options
            .push(endpoint)
            .expect("single SD option exceeds capacity");
        Self {
            flags: Flags::new_sd(false),
            entries,
            options,
        }
    }

    /// # Panics
    /// Panics if `service_ids` has more than `E` elements.
    #[must_use]
    pub fn new_find_services(reboot: bool, service_ids: &[u16]) -> Self {
        let mut entries = SdEntries::new();
        for service_id in service_ids {
            entries
                .push(Entry::FindService(ServiceEntry::find(*service_id)))
                .expect("too many service IDs for SD header");
        }
        Self {
            flags: Flags::new_sd(reboot),
            entries,
            options: SdOptions::new(),
        }
    }

    /// Creates an SD header for subscribing to an event group with an IPv4 endpoint.
    ///
    /// # Panics
    ///
    /// Panics if the const generic `E` or `O` is zero (cannot hold a single entry/option).
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new_subscription(
        service_id: u16,
        instance_id: u16,
        major_version: u8,
        ttl: u32,
        event_group_id: u16,
        client_ip: Ipv4Addr,
        protocol: TransportProtocol,
        client_port: u16,
    ) -> Self {
        let entry = Entry::SubscribeEventGroup(EventGroupEntry::new(
            service_id,
            instance_id,
            major_version,
            ttl,
            event_group_id,
        ));
        let endpoint = Options::IpV4Endpoint {
            ip: client_ip,
            protocol,
            port: client_port,
        };
        let mut entries = SdEntries::new();
        let mut options = SdOptions::new();
        entries
            .push(entry)
            .expect("single SD entry exceeds capacity");
        options
            .push(endpoint)
            .expect("single SD option exceeds capacity");
        Self {
            flags: Flags::new_sd(false),
            entries,
            options,
        }
    }

    /// Creates an SD header acknowledging an event group subscription.
    ///
    /// # Panics
    ///
    /// Panics if the const generic `E` is zero (cannot hold a single entry).
    #[must_use]
    pub fn subscribe_ack(
        service_id: u16,
        instance_id: u16,
        major_version: u8,
        ttl: u32,
        event_group_id: u16,
    ) -> Self {
        let entry = Entry::SubscribeAckEventGroup(EventGroupEntry::new(
            service_id,
            instance_id,
            major_version,
            ttl,
            event_group_id,
        ));
        let mut entries = SdEntries::new();
        entries
            .push(entry)
            .expect("single SD entry exceeds capacity");
        Self {
            flags: Flags::new_sd(true),
            entries,
            options: SdOptions::new(),
        }
    }
}

/// Zero-copy view into an SD header payload.
///
/// Created by [`SdHeaderView::parse`], which fully validates the SD header,
/// entries, and options upfront. This makes the entry and option iterators
/// infallible.
#[derive(Clone, Copy, Debug)]
pub struct SdHeaderView<'a> {
    flags: Flags,
    entries_buf: &'a [u8],
    options_buf: &'a [u8],
}

impl<'a> SdHeaderView<'a> {
    /// Parse and fully validate an SD header from `buf`.
    ///
    /// Validates:
    /// - Buffer has enough data for flags + `entries_size` + entries + `options_size` + options
    /// - `entries_size` is a multiple of `ENTRY_SIZE` (16)
    /// - All entry type bytes are valid
    /// - All options have valid types and lengths
    ///
    /// # Errors
    ///
    /// Returns an error if the buffer is too short, `entries_size` is not a multiple of 16,
    /// any entry type byte is invalid, or any option has an invalid type or length.
    pub fn parse(buf: &'a [u8]) -> Result<Self, crate::protocol::Error> {
        // Minimum: 4 (flags+reserved) + 4 (entries_size) + 4 (options_size) = 12
        if buf.len() < 12 {
            return Err(crate::protocol::Error::UnexpectedEof);
        }

        let flags = Flags::from(buf[0]);
        // bytes [1..4] are reserved

        let entries_size = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;

        if !entries_size.is_multiple_of(ENTRY_SIZE) {
            return Err(super::Error::IncorrectEntriesSize(entries_size).into());
        }

        // Need entries data + 4 bytes for options_size field
        if buf.len() < 8 + entries_size + 4 {
            return Err(crate::protocol::Error::UnexpectedEof);
        }

        let entries_buf = &buf[8..8 + entries_size];

        // Validate all entry type bytes
        let mut offset = 0;
        while offset < entries_size {
            EntryType::try_from(entries_buf[offset])?;
            offset += ENTRY_SIZE;
        }

        let options_size_offset = 8 + entries_size;
        let options_size = u32::from_be_bytes([
            buf[options_size_offset],
            buf[options_size_offset + 1],
            buf[options_size_offset + 2],
            buf[options_size_offset + 3],
        ]) as usize;

        let options_start = options_size_offset + 4;
        if buf.len() < options_start + options_size {
            return Err(crate::protocol::Error::UnexpectedEof);
        }

        let options_buf = &buf[options_start..options_start + options_size];

        // Validate all options
        let mut opt_offset = 0;
        while opt_offset < options_size {
            let remaining = &options_buf[opt_offset..];
            let wire_size = validate_option(remaining)?;
            opt_offset += wire_size;
        }

        Ok(Self {
            flags,
            entries_buf,
            options_buf,
        })
    }

    /// Returns the SD flags.
    #[must_use]
    pub fn flags(&self) -> Flags {
        self.flags
    }

    /// Returns an iterator over the SD entries.
    #[must_use]
    pub fn entries(&self) -> EntryIter<'a> {
        EntryIter::new(self.entries_buf)
    }

    /// Returns an iterator over the SD options.
    #[must_use]
    pub fn options(&self) -> OptionIter<'a> {
        OptionIter::new(self.options_buf)
    }

    /// Returns the number of entries in this SD header.
    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.entries_buf.len() / ENTRY_SIZE
    }

    /// Convert to an owned `sd::Header<E, O>`.
    ///
    /// # Errors
    ///
    /// Returns an error if there are more entries than `E` or more options than `O`,
    /// or if any entry or option cannot be decoded.
    pub fn to_owned<const E: usize, const O: usize>(
        &self,
    ) -> Result<Header<E, O>, crate::protocol::Error> {
        let mut entries = SdEntries::<E>::new();
        for entry_view in self.entries() {
            entries
                .push(entry_view.to_owned()?)
                .map_err(|_| super::Error::TooManyEntries)?;
        }
        let mut options = SdOptions::<O>::new();
        for option_view in self.options() {
            options
                .push(option_view.to_owned()?)
                .map_err(|_| super::Error::TooManyOptions)?;
        }
        Ok(Header {
            flags: self.flags,
            entries,
            options,
        })
    }
}

impl<const E: usize, const O: usize> WireFormat for Header<E, O> {
    fn required_size(&self) -> usize {
        let mut size = 12 + self.entries.len() * ENTRY_SIZE;
        for option in &self.options {
            size += option.size();
        }
        size
    }

    fn encode<T: embedded_io::Write>(
        &self,
        writer: &mut T,
    ) -> Result<usize, crate::protocol::Error> {
        writer.write_u8(u8::from(self.flags))?;
        let reserved: [u8; 3] = [0; 3];
        writer.write_bytes(&reserved)?;
        let entries_size = u32::try_from(self.entries.len() * 16).expect("entries size fits u32");
        writer.write_u32_be(entries_size)?;
        for entry in &self.entries {
            entry.encode(writer)?;
        }
        let mut options_size = 0;
        for option in &self.options {
            options_size += option.size();
        }
        writer.write_u32_be(u32::try_from(options_size).expect("options size fits u32"))?;
        for option in &self.options {
            option.write(writer)?;
        }
        Ok(12 + entries_size as usize + options_size)
    }
}

#[cfg(test)]
mod tests {
    use core::net::Ipv4Addr;

    use super::*;
    use crate::{protocol::sd::Error as SdError, traits::WireFormat};

    fn ipv4_endpoint_bytes(ip: [u8; 4], protocol: u8, port: u16) -> [u8; 12] {
        let mut b = [0u8; 12];
        b[0] = 0x00;
        b[1] = 0x09; // length = 9 (size - 3)
        b[2] = 0x04; // type = IpV4Endpoint
        b[3] = 0x00; // discard flag = 0
        b[4..8].copy_from_slice(&ip);
        b[8] = 0x00; // reserved
        b[9] = protocol;
        b[10] = (port >> 8) as u8;
        b[11] = (port & 0xFF) as u8;
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
        let flags = Flags::new_sd(true);
        let entries: SdEntries<1> = SdEntries::new();
        let options: SdOptions<1> = SdOptions::new();
        let h = Header::new(flags, entries.clone(), options.clone());
        assert_eq!(h.flags, flags);
        assert_eq!(h.entries, entries);
        assert_eq!(h.options, options);
    }

    #[test]
    fn new_service_offer_round_trips() {
        let ip = Ipv4Addr::new(192, 168, 1, 10);
        let h: Header<1, 1> = Header::new_service_offer(
            0x1234,
            0x0001,
            1,
            0,
            0xFFFFFF,
            ip,
            TransportProtocol::Udp,
            30509,
        );
        assert_eq!(h.required_size(), 40);
        let mut buf = [0u8; 64];
        h.encode(&mut buf.as_mut_slice()).unwrap();
        let view = SdHeaderView::parse(&buf[..h.required_size()]).unwrap();
        let decoded: Header<1, 1> = view.to_owned().unwrap();
        assert_eq!(decoded, h);
    }

    #[test]
    fn subscribe_ack_round_trips() {
        let h: Header<1, 0> = Header::subscribe_ack(0xAAAA, 0x0001, 1, 0xFFFFFF, 0x0010);
        assert_eq!(h.required_size(), 28);
        let mut buf = [0u8; 32];
        h.encode(&mut buf.as_mut_slice()).unwrap();
        let view = SdHeaderView::parse(&buf[..h.required_size()]).unwrap();
        let decoded: Header<1, 0> = view.to_owned().unwrap();
        assert_eq!(decoded, h);
    }

    // --- parse with exactly-sized slice ---

    #[test]
    fn parse_exact_size_slice_succeeds() {
        let h: Header<1, 1> = Header::new_service_offer(
            0x1234,
            0x0001,
            1,
            0,
            0xFFFFFF,
            Ipv4Addr::new(192, 168, 1, 10),
            TransportProtocol::Udp,
            30509,
        );
        let mut buf = [0u8; 64];
        let n = h.encode(&mut buf.as_mut_slice()).unwrap();
        // Pass exactly n bytes — no extra data beyond the SD header
        let view = SdHeaderView::parse(&buf[..n]).unwrap();
        let decoded: Header<1, 1> = view.to_owned().unwrap();
        assert_eq!(decoded, h);
    }

    #[test]
    fn parse_options_size_below_minimum_returns_error() {
        // options_size = 2 < MIN_OPTION_SIZE (4): error triggered before any option read
        let prefix = raw_header(0, 2);
        // Extend buffer so it's large enough for the declared options_size
        let mut buf = [0u8; 14];
        buf[..12].copy_from_slice(&prefix);
        assert!(matches!(
            SdHeaderView::parse(&buf),
            Err(crate::protocol::Error::Sd(SdError::IncorrectOptionsSize(2)))
        ));
    }

    #[test]
    fn parse_option_size_exceeds_declared_remaining_returns_error() {
        // options_size = 5 but IpV4Endpoint occupies 12 bytes → 12 > 5
        let prefix = raw_header(0, 5);
        let option = ipv4_endpoint_bytes([127, 0, 0, 1], 0x11, 1234);
        let mut buf = [0u8; 24];
        buf[..12].copy_from_slice(&prefix);
        buf[12..24].copy_from_slice(&option);
        assert!(matches!(
            SdHeaderView::parse(&buf),
            Err(crate::protocol::Error::Sd(SdError::IncorrectOptionsSize(5)))
        ));
    }

    #[test]
    fn parse_too_many_entries_returns_error() {
        // Encode with capacity 2, then parse and to_owned with capacity 1
        let h: Header<2, 0> = Header::new_find_services(false, &[0x0001, 0x0002]);
        let mut buf = [0u8; 64];
        h.encode(&mut buf.as_mut_slice()).unwrap();
        let view = SdHeaderView::parse(&buf[..h.required_size()]).unwrap();
        assert!(matches!(
            view.to_owned::<1, 0>(),
            Err(crate::protocol::Error::Sd(SdError::TooManyEntries))
        ));
    }

    #[test]
    fn parse_too_many_options_returns_error() {
        // Two IpV4Endpoint options (24 bytes total); to_owned with capacity 1 → TooManyOptions
        let prefix = raw_header(0, 24);
        let option = ipv4_endpoint_bytes([192, 168, 0, 1], 0x11, 8080);
        let mut buf = [0u8; 36];
        buf[..12].copy_from_slice(&prefix);
        buf[12..24].copy_from_slice(&option);
        buf[24..36].copy_from_slice(&option);
        let view = SdHeaderView::parse(&buf).unwrap();
        assert!(matches!(
            view.to_owned::<0, 1>(),
            Err(crate::protocol::Error::Sd(SdError::TooManyOptions))
        ));
    }

    // --- SdHeaderView accessors ---

    #[test]
    fn sd_header_view_entry_count() {
        let h: Header<2, 0> = Header::new_find_services(false, &[0x0001, 0x0002]);
        let mut buf = [0u8; 64];
        h.encode(&mut buf.as_mut_slice()).unwrap();
        let view = SdHeaderView::parse(&buf[..h.required_size()]).unwrap();
        assert_eq!(view.entry_count(), 2);
    }

    #[test]
    fn sd_header_view_flags() {
        let h: Header<0, 0> = Header::new_find_services(true, &[]);
        let mut buf = [0u8; 16];
        h.encode(&mut buf.as_mut_slice()).unwrap();
        let view = SdHeaderView::parse(&buf[..h.required_size()]).unwrap();
        assert_eq!(view.flags(), h.flags);
    }

    #[test]
    fn parse_incorrect_entries_size_returns_error() {
        // entries_size = 5 (not a multiple of 16)
        let mut buf = [0u8; 12];
        buf[4..8].copy_from_slice(&5u32.to_be_bytes());
        assert!(matches!(
            SdHeaderView::parse(&buf),
            Err(crate::protocol::Error::Sd(SdError::IncorrectEntriesSize(5)))
        ));
    }
}
