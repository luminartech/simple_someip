use std::io::{Read, Write};

use crate::protocol::Error;

pub enum ServiceType {
    FindService,
    OfferService,
    StopOfferService,
}

impl TryFrom<u8> for ServiceType {
    type Error = Error;
    fn try_from(value: u8) -> Result<Self, Error> {
        match value {
            0x00 => Ok(ServiceType::FindService),
            0x01 => Ok(ServiceType::OfferService),
            0x02 => Ok(ServiceType::StopOfferService),
            _ => panic!("Invalid service type: {}", value),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceEntry {
    service_type: ServiceType,
}

impl ServiceEntry {
    pub fn write<T: Write>(&self, writer: &mut T) -> Result<usize, Error> {
        Ok(0)
    }

    pub fn read<T: Read>(message_bytes: &mut T) -> Result<Self, Error> {
        let header = Header::read(message_bytes)?;
        let mut payload = vec![0; (header.length - 8) as usize];
        message_bytes.read_exact(&mut payload)?;
        Ok(Self::new(header, payload))
    }
}
