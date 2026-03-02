use core::net::Ipv4Addr;

use crate::protocol::byte_order::{ReadBytesExt, WriteBytesExt};

use crate::{protocol::Error, traits::WireFormat};

use super::{
    Entry, EventGroupEntry, Flags, Options, ServiceEntry, TransportProtocol,
    entry::{ENTRY_SIZE, OptionsCount},
};

/// Default maximum number of SD entries in a single header.
pub const MAX_SD_ENTRIES: usize = 1;
/// Default maximum number of SD options in a single header.
pub const MAX_SD_OPTIONS: usize = 1;

pub type SdEntries<const N: usize = MAX_SD_ENTRIES> = heapless::Vec<Entry, N>;
pub type SdOptions<const N: usize = MAX_SD_OPTIONS> = heapless::Vec<Options, N>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Header<
    const MAX_ENTRIES: usize = MAX_SD_ENTRIES,
    const MAX_OPTIONS: usize = MAX_SD_OPTIONS,
> {
    pub flags: Flags,
    pub entries: SdEntries<MAX_ENTRIES>,
    pub options: SdOptions<MAX_OPTIONS>,
}

impl<const E: usize, const O: usize> Header<E, O> {
    #[must_use]
    pub fn new(flags: Flags, entries: SdEntries<E>, options: SdOptions<O>) -> Self {
        Self {
            flags,
            entries,
            options,
        }
    }

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

#[cfg(test)]
mod tests {
    use core::net::Ipv4Addr;

    use super::*;
    use crate::{protocol::Error, traits::WireFormat};

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
        // required_size: 12 (overhead) + 16 (entry) + 12 (IpV4Endpoint option) = 40
        assert_eq!(h.required_size(), 40);
        let mut buf = [0u8; 64];
        h.encode(&mut buf.as_mut_slice()).unwrap();
        let decoded = Header::<1, 1>::decode(&mut &buf[..h.required_size()]).unwrap();
        assert_eq!(decoded, h);
    }

    #[test]
    fn subscribe_ack_round_trips() {
        let h: Header<1, 0> = Header::subscribe_ack(0xAAAA, 0x0001, 1, 0xFFFFFF, 0x0010);
        // required_size: 12 (overhead) + 16 (entry) = 28
        assert_eq!(h.required_size(), 28);
        let mut buf = [0u8; 32];
        h.encode(&mut buf.as_mut_slice()).unwrap();
        let decoded = Header::<1, 0>::decode(&mut &buf[..h.required_size()]).unwrap();
        assert_eq!(decoded, h);
    }

    #[test]
    fn decode_options_size_below_minimum_returns_error() {
        // options_size = 2 < MIN_OPTION_SIZE (4): error triggered before any option read
        let prefix = raw_header(0, 2);
        assert!(matches!(
            Header::<1, 1>::decode(&mut &prefix[..]),
            Err(Error::IncorrectOptionsSize(2))
        ));
    }

    #[test]
    fn decode_option_size_exceeds_declared_remaining_returns_error() {
        // options_size = 5 but IpV4Endpoint occupies 12 bytes → 12 > 5
        let prefix = raw_header(0, 5);
        let option = ipv4_endpoint_bytes([127, 0, 0, 1], 0x11, 1234);
        let mut buf = [0u8; 24];
        buf[..12].copy_from_slice(&prefix);
        buf[12..24].copy_from_slice(&option);
        assert!(matches!(
            Header::<1, 1>::decode(&mut &buf[..]),
            Err(Error::IncorrectOptionsSize(5))
        ));
    }

    #[test]
    fn decode_too_many_entries_returns_error() {
        // Encode with capacity 2, then decode with capacity 1
        let h: Header<2, 0> = Header::new_find_services(false, &[0x0001, 0x0002]);
        let mut buf = [0u8; 64];
        h.encode(&mut buf.as_mut_slice()).unwrap();
        assert!(matches!(
            Header::<1, 0>::decode(&mut &buf[..h.required_size()]),
            Err(Error::TooManyEntries)
        ));
    }

    #[test]
    fn decode_too_many_options_returns_error() {
        // Two IpV4Endpoint options (24 bytes total); decode with capacity 1 → TooManyOptions
        let prefix = raw_header(0, 24);
        let option = ipv4_endpoint_bytes([192, 168, 0, 1], 0x11, 8080);
        let mut buf = [0u8; 36];
        buf[..12].copy_from_slice(&prefix);
        buf[12..24].copy_from_slice(&option);
        buf[24..36].copy_from_slice(&option);
        assert!(matches!(
            Header::<0, 1>::decode(&mut &buf[..]),
            Err(Error::TooManyOptions)
        ));
    }
}

impl<const E: usize, const O: usize> WireFormat for Header<E, O> {
    fn decode<T: embedded_io::Read>(reader: &mut T) -> Result<Self, crate::protocol::Error> {
        const MIN_OPTION_SIZE: usize = 4;
        let flags = Flags::from(reader.read_u8()?);
        let mut reserved: [u8; 3] = [0; 3];
        reader.read_bytes(&mut reserved)?;
        let entries_size = reader.read_u32_be()?;
        let entries_count = entries_size / u32::try_from(ENTRY_SIZE).expect("constant fits u32");
        let mut entries = SdEntries::new();
        for _i in 0..entries_count {
            entries
                .push(Entry::decode(reader)?)
                .map_err(|_| Error::TooManyEntries)?;
        }

        let mut remaining_options_size = reader.read_u32_be()? as usize;
        let mut options = SdOptions::new();
        // Minimum SD option wire size: length(2) + type(1) + reserved(1) = 4 bytes
        while remaining_options_size > 0 {
            if remaining_options_size < MIN_OPTION_SIZE {
                return Err(Error::IncorrectOptionsSize(remaining_options_size));
            }
            let option = Options::read(reader)?;
            let option_size = option.size();
            if option_size > remaining_options_size {
                return Err(Error::IncorrectOptionsSize(remaining_options_size));
            }
            remaining_options_size -= option_size;
            options.push(option).map_err(|_| Error::TooManyOptions)?;
        }
        Ok(Self {
            flags,
            entries,
            options,
        })
    }

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
