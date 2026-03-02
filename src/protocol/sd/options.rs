use core::net::Ipv4Addr;

use crate::protocol::{
    Error,
    byte_order::{ReadBytesExt, WriteBytesExt},
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
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
        ip: Ipv4Addr,
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
    #[must_use]
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

    pub fn write<T: embedded_io::Write>(&self, writer: &mut T) -> Result<usize, Error> {
        writer.write_u16_be(u16::try_from(self.size() - 3).expect("option size fits u16"))?;
        match self {
            Options::Configuration => todo!("Options::Configuration not implemented"),
            Options::LoadBalancing => todo!("Options::Configuration not implemented"),
            Options::IpV4Endpoint { ip, protocol, port } => {
                writer.write_u8(u8::from(OptionType::IpV4Endpoint))?;
                writer.write_u8(0)?;
                writer.write_u32_be(ip.to_bits())?;
                writer.write_u8(0)?;
                writer.write_u8(u8::from(*protocol))?;
                writer.write_u16_be(*port)?;
                Ok(12)
            }
            Options::IpV6Endpoint => todo!("Options::Configuration not implemented"),
            Options::IpV4Multicast => todo!("Options::Configuration not implemented"),
            Options::IpV6Multicast => todo!("Options::Configuration not implemented"),
            Options::IpV4SD => todo!("Options::Configuration not implemented"),
            Options::IpV6SD => todo!("Options::Configuration not implemented"),
        }
    }

    pub fn read<T: embedded_io::Read>(message_bytes: &mut T) -> Result<Self, Error> {
        let length = message_bytes.read_u16_be()?;
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
                let ip = Ipv4Addr::from_bits(message_bytes.read_u32_be()?);
                let reserved = message_bytes.read_u8()?;
                assert!(reserved == 0, "Reserved byte not zero");
                let protocol = TransportProtocol::try_from(message_bytes.read_u8()?)?;
                let port = message_bytes.read_u16_be()?;
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

#[cfg(test)]
mod tests {
    use core::net::Ipv4Addr;

    use super::*;
    use crate::protocol::Error;

    // --- TransportProtocol ---

    #[test]
    fn transport_protocol_tcp_round_trip() {
        assert_eq!(
            TransportProtocol::try_from(0x06).unwrap(),
            TransportProtocol::Tcp
        );
        assert_eq!(u8::from(TransportProtocol::Tcp), 0x06);
    }

    #[test]
    fn transport_protocol_invalid_returns_error() {
        assert!(matches!(
            TransportProtocol::try_from(0xFF),
            Err(Error::InvalidSDOptionTransportProtocol(0xFF))
        ));
    }

    // --- Options::read: success paths ---

    #[test]
    fn options_read_ipv4_endpoint_tcp() {
        let buf: [u8; 12] = [
            0x00, 0x09, // length = 9
            0x04, // type = IpV4Endpoint
            0x00, // discard flag
            192, 168, 0, 1,    // ip
            0x00, // reserved
            0x06, // protocol = TCP
            0x04, 0xD2, // port = 1234
        ];
        let opt = Options::read(&mut &buf[..]).unwrap();
        assert_eq!(
            opt,
            Options::IpV4Endpoint {
                ip: Ipv4Addr::new(192, 168, 0, 1),
                protocol: TransportProtocol::Tcp,
                port: 1234,
            }
        );
    }

    #[test]
    fn options_read_invalid_option_type_returns_error() {
        let buf: [u8; 4] = [0x00, 0x00, 0xFF, 0x00]; // type = 0xFF (invalid)
        assert!(matches!(
            Options::read(&mut &buf[..]),
            Err(Error::InvalidSDOptionType(0xFF))
        ));
    }

    #[test]
    fn options_read_invalid_protocol_returns_error() {
        let buf: [u8; 12] = [
            0x00, 0x09, // length = 9
            0x04, // type = IpV4Endpoint
            0x00, // discard flag
            127, 0, 0, 1,    // ip
            0x00, // reserved
            0xAB, // invalid protocol
            0x00, 0x50, // port = 80
        ];
        assert!(matches!(
            Options::read(&mut &buf[..]),
            Err(Error::InvalidSDOptionTransportProtocol(0xAB))
        ));
    }

    // --- Options::size: todo! branches (std only — requires catch_unwind) ---

    #[test]
    #[should_panic(expected = "not yet implemented")]
    fn options_size_configuration_panics() {
        let _ = Options::Configuration.size();
    }

    #[test]
    #[should_panic(expected = "not yet implemented")]
    fn options_size_load_balancing_panics() {
        let _ = Options::LoadBalancing.size();
    }

    #[test]
    #[should_panic(expected = "not yet implemented")]
    fn options_size_ipv6_endpoint_panics() {
        let _ = Options::IpV6Endpoint.size();
    }

    #[test]
    #[should_panic(expected = "not yet implemented")]
    fn options_size_ipv4_multicast_panics() {
        let _ = Options::IpV4Multicast.size();
    }

    #[test]
    #[should_panic(expected = "not yet implemented")]
    fn options_size_ipv6_multicast_panics() {
        let _ = Options::IpV6Multicast.size();
    }

    #[test]
    #[should_panic(expected = "not yet implemented")]
    fn options_size_ipv4_sd_panics() {
        let _ = Options::IpV4SD.size();
    }

    #[test]
    #[should_panic(expected = "not yet implemented")]
    fn options_size_ipv6_sd_panics() {
        let _ = Options::IpV6SD.size();
    }

    // --- Options::read: todo! branches (std only — requires catch_unwind) ---

    #[test]
    #[should_panic(expected = "not yet implemented")]
    fn options_read_configuration_panics() {
        let buf: [u8; 4] = [0x00, 0x00, 0x01, 0x00];
        let _ = Options::read(&mut &buf[..]);
    }

    #[test]
    #[should_panic(expected = "not yet implemented")]
    fn options_read_load_balancing_panics() {
        let buf: [u8; 4] = [0x00, 0x00, 0x02, 0x00];
        let _ = Options::read(&mut &buf[..]);
    }

    #[test]
    #[should_panic(expected = "not yet implemented")]
    fn options_read_ipv6_endpoint_panics() {
        let buf: [u8; 4] = [0x00, 0x00, 0x06, 0x00];
        let _ = Options::read(&mut &buf[..]);
    }

    #[test]
    #[should_panic(expected = "not yet implemented")]
    fn options_read_ipv4_multicast_panics() {
        let buf: [u8; 4] = [0x00, 0x00, 0x14, 0x00];
        let _ = Options::read(&mut &buf[..]);
    }

    #[test]
    #[should_panic(expected = "not yet implemented")]
    fn options_read_ipv6_multicast_panics() {
        let buf: [u8; 4] = [0x00, 0x00, 0x16, 0x00];
        let _ = Options::read(&mut &buf[..]);
    }

    #[test]
    #[should_panic(expected = "not yet implemented")]
    fn options_read_ipv4_sd_panics() {
        let buf: [u8; 4] = [0x00, 0x00, 0x24, 0x00];
        let _ = Options::read(&mut &buf[..]);
    }

    #[test]
    #[should_panic(expected = "not yet implemented")]
    fn options_read_ipv6_sd_panics() {
        let buf: [u8; 4] = [0x00, 0x00, 0x26, 0x00];
        let _ = Options::read(&mut &buf[..]);
    }
}
