use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use std::{net::Ipv4Addr, vec};

use crate::{protocol::Error, traits::WireFormat};

use super::{
    Entry, EventGroupEntry, Flags, Options, ServiceEntry, TransportProtocol,
    entry::{ENTRY_SIZE, OptionsCount},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Header {
    pub flags: Flags,
    pub entries: Vec<Entry>,
    pub options: Vec<Options>,
}

impl Header {
    pub fn new(flags: Flags, entries: Vec<Entry>, options: Vec<Options>) -> Self {
        Self {
            flags,
            entries,
            options,
        }
    }

    pub fn new_service_offer(
        reboot: bool,
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
        Self {
            flags: Flags::new_sd(reboot),
            entries: vec![entry],
            options: vec![endpoint],
        }
    }

    pub fn new_find_services(reboot: bool, service_ids: Vec<u16>) -> Self {
        let entries = service_ids
            .iter()
            .map(|service_id| Entry::FindService(ServiceEntry::find(*service_id)))
            .collect();
        Self {
            flags: Flags::new_sd(reboot),
            entries,
            options: vec![],
        }
    }

    pub fn new_subscription(
        reboot: bool,
        service_id: u16,
        instance_id: u16,
        major_version: u8,
        ttl: u32,
        counter: u8,
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
            counter,
            event_group_id,
        ));
        let endpoint = Options::IpV4Endpoint {
            ip: client_ip,
            protocol,
            port: client_port,
        };
        Self {
            flags: Flags::new_sd(reboot),
            entries: vec![entry],
            options: vec![endpoint],
        }
    }

    pub fn subscribe_ack(
        service_id: u16,
        instance_id: u16,
        major_version: u8,
        ttl: u32,
        counter: u8,
        event_group_id: u16,
    ) -> Self {
        let entry = Entry::SubscribeAckEventGroup(EventGroupEntry::new(
            service_id,
            instance_id,
            major_version,
            ttl,
            counter,
            event_group_id,
        ));
        Self {
            flags: Flags::new_sd(true),
            entries: vec![entry],
            options: vec![],
        }
    }
}

impl WireFormat for Header {
    fn from_reader<T: std::io::Read>(reader: &mut T) -> Result<Self, crate::protocol::Error> {
        let flags = Flags::from(reader.read_u8()?);
        let mut reserved: [u8; 3] = [0; 3];
        reader.read_exact(&mut reserved)?;
        let entries_size = reader.read_u32::<BigEndian>()?;
        let entries_count = entries_size / ENTRY_SIZE as u32;
        let mut entries = Vec::with_capacity(entries_count as usize);
        let options_count = 0;
        for _i in 0..entries_count {
            entries.push(Entry::from_reader(reader)?);
        }

        let mut remaining_options_size = reader.read_u32::<BigEndian>()? as usize;
        let mut options = Vec::with_capacity(options_count as usize);
        while remaining_options_size > 0 {
            options.push(Options::read(reader)?);
            remaining_options_size -= options.last().unwrap().size();
        }
        if remaining_options_size != 0 {
            return Err(Error::IncorrectOptionsSize(remaining_options_size));
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

    fn to_writer<T: std::io::Write>(
        &self,
        writer: &mut T,
    ) -> Result<usize, crate::protocol::Error> {
        writer.write_u8(u8::from(self.flags))?;
        let reserved: [u8; 3] = [0; 3];
        writer.write_all(&reserved)?;
        let entries_size = (self.entries.len() * 16) as u32;
        writer.write_u32::<BigEndian>(entries_size)?;
        for entry in &self.entries {
            entry.to_writer(writer)?;
        }
        let mut options_size = 0;
        for option in &self.options {
            options_size += option.size();
        }
        writer.write_u32::<BigEndian>(options_size as u32)?;
        for option in &self.options {
            option.write(writer)?;
        }
        Ok(12 + entries_size as usize + options_size)
    }
}
