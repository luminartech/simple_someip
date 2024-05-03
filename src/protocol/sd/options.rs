use std::io::{Read, Write};

use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};

use crate::protocol::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportProtocol {
    Udp,
    Tcp,
}

impl TryFrom<u8> for TransportProtocol {
    type Error = Error;
    fn try_from(value: u8) -> Result<Self, Error> {
        match value {
            0x11 => Ok(TransportProtocol::Udp),
            0x06 => Ok(TransportProtocol::Tcp),
            _ => Err(Error::InvalidSDOptionTransportProtocol(value)),
        }
    }
}

impl From<TransportProtocol> for u8 {
    fn from(transport_protocol: TransportProtocol) -> u8 {
        match transport_protocol {
            TransportProtocol::Udp => 0x11,
            TransportProtocol::Tcp => 0x06,
        }
    }
}

enum OptionType {
    Configuration,
    LoadBalancing,
    IpV4Endpoint,
    IpV6Endpoint,
    IpV4Multicast,
    IpV6Multicast,
    IpV4SD,
    IpV6SD,
}

impl TryFrom<u8> for OptionType {
    type Error = Error;
    fn try_from(value: u8) -> Result<Self, Error> {
        match value {
            0x01 => Ok(OptionType::Configuration),
            0x02 => Ok(OptionType::LoadBalancing),
            0x04 => Ok(OptionType::IpV4Endpoint),
            0x06 => Ok(OptionType::IpV6Endpoint),
            0x14 => Ok(OptionType::IpV4Multicast),
            0x16 => Ok(OptionType::IpV6Multicast),
            0x24 => Ok(OptionType::IpV4SD),
            0x26 => Ok(OptionType::IpV6SD),
            _ => Err(Error::InvalidSDOptionType(value)),
        }
    }
}

impl From<OptionType> for u8 {
    fn from(option_type: OptionType) -> u8 {
        match option_type {
            OptionType::Configuration => 0x01,
            OptionType::LoadBalancing => 0x02,
            OptionType::IpV4Endpoint => 0x04,
            OptionType::IpV6Endpoint => 0x06,
            OptionType::IpV4Multicast => 0x14,
            OptionType::IpV6Multicast => 0x16,
            OptionType::IpV4SD => 0x24,
            OptionType::IpV6SD => 0x26,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Options {
    Configuration,
    LoadBalancing,
    IpV4Endpoint {
        ip: u32,
        protocol: TransportProtocol,
        port: u16,
    },
    IpV6Endpoint,
    IpV4Multicast,
    IpV6Multicast,
    IpV4SD,
    IpV6SD,
}

impl Options {
    pub fn size(&self) -> usize {
        match self {
            Options::Configuration => todo!("Options::Configuration not implemented"),
            Options::LoadBalancing => todo!("Options::Configuration not implemented"),
            Options::IpV4Endpoint { .. } => 12,
            Options::IpV6Endpoint => todo!("Options::Configuration not implemented"),
            Options::IpV4Multicast => todo!("Options::Configuration not implemented"),
            Options::IpV6Multicast => todo!("Options::Configuration not implemented"),
            Options::IpV4SD => todo!("Options::Configuration not implemented"),
            Options::IpV6SD => todo!("Options::Configuration not implemented"),
        }
    }

    pub fn write<T: Write>(&self, writer: &mut T) -> Result<usize, Error> {
        writer.write_u16::<BigEndian>((self.size() - 3) as u16)?;
        match self {
            Options::Configuration => todo!("Options::Configuration not implemented"),
            Options::LoadBalancing => todo!("Options::Configuration not implemented"),
            Options::IpV4Endpoint { ip, protocol, port } => {
                writer.write_u8(u8::from(OptionType::IpV4Endpoint))?;
                writer.write_u8(0)?;
                writer.write_u32::<BigEndian>(*ip)?;
                writer.write_u8(0)?;
                writer.write_u8(u8::from(*protocol))?;
                writer.write_u16::<BigEndian>(*port)?;
                Ok(12)
            }
            Options::IpV6Endpoint => todo!("Options::Configuration not implemented"),
            Options::IpV4Multicast => todo!("Options::Configuration not implemented"),
            Options::IpV6Multicast => todo!("Options::Configuration not implemented"),
            Options::IpV4SD => todo!("Options::Configuration not implemented"),
            Options::IpV6SD => todo!("Options::Configuration not implemented"),
        }
    }

    pub fn read<T: Read>(message_bytes: &mut T) -> Result<Self, Error> {
        let length = message_bytes.read_u16::<BigEndian>()?;
        let option_type = OptionType::try_from(message_bytes.read_u8()?)?;
        let discard_flag = message_bytes.read_u8()? & 0x80 != 0;

        match option_type {
            OptionType::Configuration => {
                todo!("Configuration option not implemented");
            }
            OptionType::LoadBalancing => {
                todo!("LoadBalancing option not implemented");
            }
            OptionType::IpV4Endpoint => {
                assert!(length == 9, "Invalid length for IpV4Endpoint");
                assert!(!discard_flag, "Discard flag not set");
                let ip = message_bytes.read_u32::<BigEndian>()?;
                let reserved = message_bytes.read_u8()?;
                assert!(reserved == 0, "Reserved byte not zero");
                let protocol = TransportProtocol::try_from(message_bytes.read_u8()?)?;
                let port = message_bytes.read_u16::<BigEndian>()?;
                Ok(Options::IpV4Endpoint { ip, protocol, port })
            }
            OptionType::IpV6Endpoint => {
                todo!("IpV6Endpoint option not implemented");
            }
            OptionType::IpV4Multicast => {
                todo!("Multicast Option not implemented");
            }
            OptionType::IpV6Multicast => {
                todo!("Multicast Option not implemented");
            }
            OptionType::IpV4SD => {
                todo!("IpV4SD Option not implemented");
            }
            OptionType::IpV6SD => {
                todo!("IpV6SD Option not implemented");
            }
        }
    }
}
