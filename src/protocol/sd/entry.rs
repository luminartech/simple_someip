use crate::{
    protocol::{
        Error,
        byte_order::{ReadBytesExt, WriteBytesExt},
    },
    traits::WireFormat,
};

pub const ENTRY_SIZE: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EntryType {
    FindService,
    OfferService,
    StopOfferService,
    Subscribe,
    SubscribeAck,
}

impl TryFrom<u8> for EntryType {
    type Error = Error;
    fn try_from(value: u8) -> Result<Self, Error> {
        match value {
            0x00 => Ok(EntryType::FindService),
            0x01 => Ok(EntryType::OfferService),
            0x02 => Ok(EntryType::StopOfferService),
            0x06 => Ok(EntryType::Subscribe),
            0x07 => Ok(EntryType::SubscribeAck),
            _ => Err(Error::InvalidSDEntryType(value)),
        }
    }
}

impl From<EntryType> for u8 {
    fn from(service_entry_type: EntryType) -> u8 {
        match service_entry_type {
            EntryType::FindService => 0x00,
            EntryType::OfferService => 0x01,
            EntryType::StopOfferService => 0x02,
            EntryType::Subscribe => 0x06,
            EntryType::SubscribeAck => 0x07,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OptionsCount {
    pub first_options_count: u8,
    pub second_options_count: u8,
}

impl From<u8> for OptionsCount {
    fn from(value: u8) -> Self {
        let first_options_count = (value & 0xf0) >> 4;
        let second_options_count = value & 0x0f;

        Self {
            first_options_count,
            second_options_count,
        }
    }
}

impl From<OptionsCount> for u8 {
    fn from(options_count: OptionsCount) -> u8 {
        ((options_count.first_options_count << 4) & 0xf0)
            | (options_count.second_options_count & 0x0f)
    }
}

impl OptionsCount {
    /// # Panics
    /// Panics if either count is >= 16 (each count must fit in a 4-bit nibble).
    #[must_use]
    pub fn new(first_options_count: u8, second_options_count: u8) -> Self {
        assert!(first_options_count < 16);
        assert!(second_options_count < 16);
        OptionsCount {
            first_options_count,
            second_options_count,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventGroupEntry {
    pub index_first_options_run: u8,
    pub index_second_options_run: u8,
    pub options_count: OptionsCount,
    pub service_id: u16,
    pub instance_id: u16,
    pub major_version: u8,
    /// ttl is a u24 value
    pub ttl: u32,
    pub counter: u16,
    pub event_group_id: u16,
}

impl EventGroupEntry {
    #[must_use]
    pub fn new(
        service_id: u16,
        instance_id: u16,
        major_version: u8,
        ttl: u32,
        event_group_id: u16,
    ) -> Self {
        Self {
            index_first_options_run: 0,
            index_second_options_run: 0,
            options_count: OptionsCount::new(1, 0),
            service_id,
            instance_id,
            major_version,
            ttl,
            counter: 0,
            event_group_id,
        }
    }
}

impl WireFormat for EventGroupEntry {
    fn decode<T: embedded_io::Read>(reader: &mut T) -> Result<Self, crate::protocol::Error> {
        let index_first_options_run = reader.read_u8()?;
        let index_second_options_run = reader.read_u8()?;
        let options_count = OptionsCount::from(reader.read_u8()?);
        let service_id = reader.read_u16_be()?;
        let instance_id = reader.read_u16_be()?;
        let major_version = reader.read_u8()?;
        let ttl = reader.read_u24_be()?;
        let counter = reader.read_u16_be()? & 0x000f;
        let event_group_id = reader.read_u16_be()?;
        Ok(Self {
            index_first_options_run,
            index_second_options_run,
            options_count,
            service_id,
            instance_id,
            major_version,
            ttl,
            counter,
            event_group_id,
        })
    }

    fn required_size(&self) -> usize {
        16
    }

    fn encode<T: embedded_io::Write>(
        &self,
        writer: &mut T,
    ) -> Result<usize, crate::protocol::Error> {
        writer.write_u8(self.index_first_options_run)?;
        writer.write_u8(self.index_second_options_run)?;
        writer.write_u8(u8::from(self.options_count))?;
        writer.write_u16_be(self.service_id)?;
        writer.write_u16_be(self.instance_id)?;
        writer.write_u8(self.major_version)?;
        writer.write_u24_be(self.ttl)?;
        writer.write_u16_be(self.counter)?;
        writer.write_u16_be(self.event_group_id)?;
        Ok(16)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceEntry {
    pub index_first_options_run: u8,
    pub index_second_options_run: u8,
    pub options_count: OptionsCount,
    pub service_id: u16,
    pub instance_id: u16,
    pub major_version: u8,
    /// ttl is a u24 value
    pub ttl: u32,
    pub minor_version: u32,
}

impl ServiceEntry {
    #[must_use]
    pub fn find(service_id: u16) -> Self {
        Self {
            index_first_options_run: 0,
            index_second_options_run: 0,
            options_count: OptionsCount::new(1, 0),
            service_id,
            instance_id: 0xFFFF,
            major_version: 0xFF,
            ttl: 0x00FF_FFFF,
            minor_version: 0xFFFF_FFFF,
        }
    }
}

impl WireFormat for ServiceEntry {
    fn decode<R: embedded_io::Read>(reader: &mut R) -> Result<Self, Error> {
        let index_first_options_run = reader.read_u8()?;
        let index_second_options_run = reader.read_u8()?;
        let options_count = OptionsCount::from(reader.read_u8()?);
        let service_id = reader.read_u16_be()?;
        let instance_id = reader.read_u16_be()?;
        let major_version = reader.read_u8()?;
        let ttl = reader.read_u24_be()?;
        let minor_version = reader.read_u32_be()?;
        Ok(Self {
            index_first_options_run,
            index_second_options_run,
            options_count,
            service_id,
            instance_id,
            major_version,
            ttl,
            minor_version,
        })
    }

    fn required_size(&self) -> usize {
        16
    }

    fn encode<W: embedded_io::Write>(&self, writer: &mut W) -> Result<usize, Error> {
        writer.write_u8(self.index_first_options_run)?;
        writer.write_u8(self.index_second_options_run)?;
        writer.write_u8(u8::from(self.options_count))?;
        writer.write_u16_be(self.service_id)?;
        writer.write_u16_be(self.instance_id)?;
        writer.write_u8(self.major_version)?;
        writer.write_u24_be(self.ttl)?;
        writer.write_u32_be(self.minor_version)?;
        Ok(16)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Entry {
    FindService(ServiceEntry),
    OfferService(ServiceEntry),
    StopOfferService(ServiceEntry),
    SubscribeEventGroup(EventGroupEntry),
    SubscribeAckEventGroup(EventGroupEntry),
}

impl Entry {
    #[must_use]
    pub fn first_options_count(&self) -> u8 {
        match self {
            Entry::FindService(service_entry)
            | Entry::OfferService(service_entry)
            | Entry::StopOfferService(service_entry) => {
                service_entry.options_count.first_options_count
            }
            Entry::SubscribeEventGroup(event_group_entry)
            | Entry::SubscribeAckEventGroup(event_group_entry) => {
                event_group_entry.options_count.first_options_count
            }
        }
    }

    #[must_use]
    pub fn second_options_count(&self) -> u8 {
        match self {
            Entry::FindService(service_entry)
            | Entry::OfferService(service_entry)
            | Entry::StopOfferService(service_entry) => {
                service_entry.options_count.second_options_count
            }
            Entry::SubscribeEventGroup(event_group_entry)
            | Entry::SubscribeAckEventGroup(event_group_entry) => {
                event_group_entry.options_count.second_options_count
            }
        }
    }

    #[must_use]
    pub fn total_options_count(&self) -> u8 {
        self.first_options_count() + self.second_options_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_entry(entry: &Entry) -> [u8; 17] {
        let mut buf = [0u8; 17];
        entry.encode(&mut buf.as_mut_slice()).unwrap();
        buf
    }

    fn make_service_entry() -> ServiceEntry {
        ServiceEntry {
            index_first_options_run: 1,
            index_second_options_run: 2,
            options_count: OptionsCount::new(3, 4),
            service_id: 0x1234,
            instance_id: 0x5678,
            major_version: 0x01,
            ttl: 0x0000_00FF,
            minor_version: 0x0000_0002,
        }
    }

    fn make_event_group_entry() -> EventGroupEntry {
        EventGroupEntry {
            index_first_options_run: 1,
            index_second_options_run: 2,
            options_count: OptionsCount::new(3, 4),
            service_id: 0xABCD,
            instance_id: 0x0001,
            major_version: 0x02,
            ttl: 0x0000_0064,
            counter: 0x0003,
            event_group_id: 0x0010,
        }
    }

    // --- EntryType ---

    #[test]
    fn entry_type_try_from_all_valid_values() {
        assert_eq!(EntryType::try_from(0x00).unwrap(), EntryType::FindService);
        assert_eq!(EntryType::try_from(0x01).unwrap(), EntryType::OfferService);
        assert_eq!(
            EntryType::try_from(0x02).unwrap(),
            EntryType::StopOfferService
        );
        assert_eq!(EntryType::try_from(0x06).unwrap(), EntryType::Subscribe);
        assert_eq!(EntryType::try_from(0x07).unwrap(), EntryType::SubscribeAck);
    }

    #[test]
    fn entry_type_try_from_invalid_returns_error() {
        assert!(matches!(
            EntryType::try_from(0x03),
            Err(Error::InvalidSDEntryType(0x03))
        ));
    }

    #[test]
    fn entry_type_into_u8_all_variants() {
        assert_eq!(u8::from(EntryType::FindService), 0x00);
        assert_eq!(u8::from(EntryType::OfferService), 0x01);
        assert_eq!(u8::from(EntryType::StopOfferService), 0x02);
        assert_eq!(u8::from(EntryType::Subscribe), 0x06);
        assert_eq!(u8::from(EntryType::SubscribeAck), 0x07);
    }

    // --- OptionsCount ---

    #[test]
    fn options_count_round_trip() {
        let oc = OptionsCount::new(3, 7);
        let byte = u8::from(oc);
        let decoded = OptionsCount::from(byte);
        assert_eq!(decoded.first_options_count, 3);
        assert_eq!(decoded.second_options_count, 7);
    }

    // --- required_size ---

    #[test]
    fn service_entry_required_size() {
        assert_eq!(make_service_entry().required_size(), 16);
    }

    #[test]
    fn event_group_entry_required_size() {
        assert_eq!(make_event_group_entry().required_size(), 16);
    }

    #[test]
    fn entry_required_size_all_variants() {
        assert_eq!(Entry::FindService(make_service_entry()).required_size(), 17);
        assert_eq!(
            Entry::OfferService(make_service_entry()).required_size(),
            17
        );
        assert_eq!(
            Entry::StopOfferService(make_service_entry()).required_size(),
            17
        );
        assert_eq!(
            Entry::SubscribeEventGroup(make_event_group_entry()).required_size(),
            17
        );
        assert_eq!(
            Entry::SubscribeAckEventGroup(make_event_group_entry()).required_size(),
            17
        );
    }

    // --- first/second/total options count ---

    #[test]
    fn entry_options_count_service_variants() {
        let se = make_service_entry(); // first=3, second=4
        for entry in [
            Entry::FindService(se),
            Entry::OfferService(make_service_entry()),
            Entry::StopOfferService(make_service_entry()),
        ] {
            assert_eq!(entry.first_options_count(), 3);
            assert_eq!(entry.second_options_count(), 4);
            assert_eq!(entry.total_options_count(), 7);
        }
    }

    #[test]
    fn entry_options_count_event_group_variants() {
        let eg = make_event_group_entry(); // first=3, second=4
        for entry in [
            Entry::SubscribeEventGroup(eg.clone()),
            Entry::SubscribeAckEventGroup(eg),
        ] {
            assert_eq!(entry.first_options_count(), 3);
            assert_eq!(entry.second_options_count(), 4);
            assert_eq!(entry.total_options_count(), 7);
        }
    }

    // --- Entry encode/decode round-trips ---

    #[test]
    fn find_service_entry_round_trips() {
        let entry = Entry::FindService(make_service_entry());
        let buf = encode_entry(&entry);
        assert_eq!(Entry::decode(&mut &buf[..]).unwrap(), entry);
    }

    #[test]
    fn offer_service_entry_round_trips() {
        let entry = Entry::OfferService(make_service_entry());
        let buf = encode_entry(&entry);
        assert_eq!(Entry::decode(&mut &buf[..]).unwrap(), entry);
    }

    #[test]
    fn stop_offer_service_entry_round_trips() {
        let entry = Entry::StopOfferService(make_service_entry());
        let buf = encode_entry(&entry);
        assert_eq!(Entry::decode(&mut &buf[..]).unwrap(), entry);
    }

    #[test]
    fn subscribe_event_group_entry_round_trips() {
        let entry = Entry::SubscribeEventGroup(make_event_group_entry());
        let buf = encode_entry(&entry);
        assert_eq!(Entry::decode(&mut &buf[..]).unwrap(), entry);
    }

    #[test]
    fn subscribe_ack_event_group_entry_round_trips() {
        let entry = Entry::SubscribeAckEventGroup(make_event_group_entry());
        let buf = encode_entry(&entry);
        assert_eq!(Entry::decode(&mut &buf[..]).unwrap(), entry);
    }

    #[test]
    fn entry_decode_invalid_type_returns_error() {
        let buf = [0x03u8; 17]; // 0x03 is not a valid EntryType
        assert!(matches!(
            Entry::decode(&mut &buf[..]),
            Err(Error::InvalidSDEntryType(0x03))
        ));
    }
}

impl WireFormat for Entry {
    fn decode<R: embedded_io::Read>(reader: &mut R) -> Result<Self, Error> {
        let entry_type = EntryType::try_from(reader.read_u8()?)?;
        match entry_type {
            EntryType::FindService => {
                let service_entry = ServiceEntry::decode(reader)?;
                Ok(Entry::FindService(service_entry))
            }
            EntryType::OfferService => {
                let service_entry = ServiceEntry::decode(reader)?;
                Ok(Entry::OfferService(service_entry))
            }
            EntryType::StopOfferService => {
                let service_entry = ServiceEntry::decode(reader)?;
                Ok(Entry::StopOfferService(service_entry))
            }
            EntryType::Subscribe => {
                let event_group_entry = EventGroupEntry::decode(reader)?;
                Ok(Entry::SubscribeEventGroup(event_group_entry))
            }
            EntryType::SubscribeAck => {
                let event_group_entry = EventGroupEntry::decode(reader)?;
                Ok(Entry::SubscribeAckEventGroup(event_group_entry))
            }
        }
    }

    fn required_size(&self) -> usize {
        1 + match self {
            Entry::FindService(service_entry)
            | Entry::OfferService(service_entry)
            | Entry::StopOfferService(service_entry) => service_entry.required_size(),
            Entry::SubscribeEventGroup(event_group_entry)
            | Entry::SubscribeAckEventGroup(event_group_entry) => event_group_entry.required_size(),
        }
    }

    fn encode<W: embedded_io::Write>(&self, writer: &mut W) -> Result<usize, Error> {
        match self {
            Entry::FindService(service_entry) => {
                writer.write_u8(u8::from(EntryType::FindService))?;
                service_entry.encode(writer)
            }
            Entry::OfferService(service_entry) => {
                writer.write_u8(u8::from(EntryType::OfferService))?;
                service_entry.encode(writer)
            }
            Entry::StopOfferService(service_entry) => {
                writer.write_u8(u8::from(EntryType::StopOfferService))?;
                service_entry.encode(writer)
            }
            Entry::SubscribeEventGroup(event_group_entry) => {
                writer.write_u8(u8::from(EntryType::Subscribe))?;
                event_group_entry.encode(writer)
            }
            Entry::SubscribeAckEventGroup(event_group_entry) => {
                writer.write_u8(u8::from(EntryType::SubscribeAck))?;
                event_group_entry.encode(writer)
            }
        }
    }
}
