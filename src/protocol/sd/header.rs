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
