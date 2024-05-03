use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use std::{
    io::{Read, Write},
    net::Ipv4Addr,
    vec,
};

use crate::{
    client,
    protocol::{self, Error},
};

use super::{
    entry::ENTRY_SIZE, Entry, EventGroupEntry, Flags, Options, ServiceEntry, TransportProtocol,
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

    pub fn new_find_services(reboot: bool, service_ids: Vec<u16>) -> Self {
        let entries = service_ids
            .iter()
            .map(|service_id| Entry::FindService(ServiceEntry::new_find(*service_id)))
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
        let entry = Entry::SubscribeEventGroup(EventGroupEntry::new_subscription(
            service_id,
            instance_id,
            major_version,
            ttl,
            counter,
            event_group_id,
        ));
        let endpoint = Options::IpV4Endpoint {
            ip: client_ip.into(),
            protocol,
            port: client_port,
        };
        Self {
            flags: Flags::new_sd(reboot),
            entries: vec![entry],
            options: vec![endpoint],
        }
    }

    pub fn size(&self) -> usize {
        let mut size = 12 + self.entries.len() * ENTRY_SIZE;
        for option in &self.options {
            size += option.size();
        }
        size
    }
    pub fn write<T: Write>(&self, writer: &mut T) -> Result<usize, Error> {
        writer.write_u8(u8::from(self.flags))?;
        let reserved: [u8; 3] = [0; 3];
        writer.write_all(&reserved)?;
        let entries_size = (self.entries.len() * 16) as u32;
        writer.write_u32::<BigEndian>(entries_size)?;
        for entry in &self.entries {
            entry.write(writer)?;
        }
        let mut options_size = 0;
        for option in &self.options {
            options_size += option.size();
        }
        writer.write_u32::<BigEndian>(options_size as u32)?;
        for option in &self.options {
            option.write(writer)?;
        }
        Ok(12 + entries_size as usize + options_size as usize)
    }

    pub fn read<T: Read>(message_bytes: &mut T) -> Result<Self, Error> {
        let flags = Flags::from(message_bytes.read_u8()?);
        let mut reserved: [u8; 3] = [0; 3];
        message_bytes.read_exact(&mut reserved)?;
        let entries_size = message_bytes.read_u32::<BigEndian>()?;
        let entries_count = entries_size / ENTRY_SIZE as u32;
        let mut entries = Vec::with_capacity(entries_count as usize);
        let mut options_count = 0;
        for i in 0..entries_count {
            entries.push(Entry::read(message_bytes)?);
            options_count += entries[i as usize].first_options_count();
        }

        let mut remaining_options_size = message_bytes.read_u32::<BigEndian>()? as usize;
        let mut options = Vec::with_capacity(options_count as usize);
        for i in 0..options_count as usize {
            options.push(Options::read(message_bytes)?);
            remaining_options_size -= options[i].size();
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
}
