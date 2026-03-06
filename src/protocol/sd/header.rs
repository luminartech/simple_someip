use crate::protocol::byte_order::WriteBytesExt;

use crate::traits::WireFormat;

use super::{
    Entry, Flags, Options,
    entry::{ENTRY_SIZE, EntryIter, EntryType},
    options::{OptionIter, validate_option},
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
    pub fn new(flags: Flags, entries: &'a [Entry], options: &'a [Options]) -> Self {
        Self {
            flags,
            entries,
            options,
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
}

impl WireFormat for Header<'_> {
    fn required_size(&self) -> usize {
        let mut size = 12 + self.entries.len() * ENTRY_SIZE;
        for option in self.options {
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
        for entry in self.entries {
            entry.encode(writer)?;
        }
        let mut options_size = 0;
        for option in self.options {
            options_size += option.size();
        }
        writer.write_u32_be(u32::try_from(options_size).expect("options size fits u32"))?;
        for option in self.options {
            option.write(writer)?;
        }
        Ok(12 + entries_size as usize + options_size)
    }
}

#[cfg(test)]
mod tests {
    use core::net::Ipv4Addr;

    use super::*;
    use crate::{
        protocol::sd::{
            Error as SdError, EventGroupEntry, OptionsCount, ServiceEntry, TransportProtocol,
        },
        traits::WireFormat,
    };

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
            ttl: 0xFFFFFF,
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
        let h = Header::new(Flags::new_sd(false), &entries, &options);
        assert_eq!(h.required_size(), 40);
        let mut buf = [0u8; 64];
        h.encode(&mut buf.as_mut_slice()).unwrap();
        let view = SdHeaderView::parse(&buf[..h.required_size()]).unwrap();
        assert_eq!(view.entry_count(), 1);
        let entry_view = view.entries().next().unwrap();
        assert_eq!(entry_view.service_id(), 0x1234);
    }

    #[test]
    fn subscribe_ack_round_trips() {
        let entry = Entry::SubscribeAckEventGroup(EventGroupEntry::new(
            0xAAAA, 0x0001, 1, 0xFFFFFF, 0x0010,
        ));
        let entries = [entry];
        let h = Header::new(Flags::new_sd(true), &entries, &[]);
        assert_eq!(h.required_size(), 28);
        let mut buf = [0u8; 32];
        h.encode(&mut buf.as_mut_slice()).unwrap();
        let view = SdHeaderView::parse(&buf[..h.required_size()]).unwrap();
        assert_eq!(view.entry_count(), 1);
    }

    #[test]
    fn parse_exact_size_slice_succeeds() {
        let entry = Entry::OfferService(ServiceEntry {
            service_id: 0x1234,
            instance_id: 0x0001,
            major_version: 1,
            ttl: 0xFFFFFF,
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
        let h = Header::new(Flags::new_sd(false), &entries, &options);
        let mut buf = [0u8; 64];
        let n = h.encode(&mut buf.as_mut_slice()).unwrap();
        let view = SdHeaderView::parse(&buf[..n]).unwrap();
        assert_eq!(view.entry_count(), 1);
    }

    #[test]
    fn parse_options_size_below_minimum_returns_error() {
        let prefix = raw_header(0, 2);
        let mut buf = [0u8; 14];
        buf[..12].copy_from_slice(&prefix);
        assert!(matches!(
            SdHeaderView::parse(&buf),
            Err(crate::protocol::Error::Sd(SdError::IncorrectOptionsSize(2)))
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
            Err(crate::protocol::Error::Sd(SdError::IncorrectOptionsSize(5)))
        ));
    }

    // --- SdHeaderView accessors ---

    #[test]
    fn sd_header_view_entry_count() {
        let entries = [
            Entry::FindService(ServiceEntry::find(0x0001)),
            Entry::FindService(ServiceEntry::find(0x0002)),
        ];
        let h = Header::new(Flags::new_sd(false), &entries, &[]);
        let mut buf = [0u8; 64];
        h.encode(&mut buf.as_mut_slice()).unwrap();
        let view = SdHeaderView::parse(&buf[..h.required_size()]).unwrap();
        assert_eq!(view.entry_count(), 2);
    }

    #[test]
    fn sd_header_view_flags() {
        let h = Header::new(Flags::new_sd(true), &[], &[]);
        let mut buf = [0u8; 16];
        h.encode(&mut buf.as_mut_slice()).unwrap();
        let view = SdHeaderView::parse(&buf[..h.required_size()]).unwrap();
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
}
