use core::net::{Ipv4Addr, Ipv6Addr};

use crate::protocol::{
    Error,
    byte_order::{ReadBytesExt, WriteBytesExt},
};

pub const MAX_CONFIGURATION_STRING_LENGTH: usize = 256;

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

// Boxing is not available in no_std, so allow the large variant.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Options {
    Configuration {
        configuration_string: heapless::Vec<u8, MAX_CONFIGURATION_STRING_LENGTH>,
    },
    LoadBalancing {
        priority: u16,
        weight: u16,
    },
    IpV4Endpoint {
        ip: Ipv4Addr,
        protocol: TransportProtocol,
        port: u16,
    },
    IpV6Endpoint {
        ip: Ipv6Addr,
        protocol: TransportProtocol,
        port: u16,
    },
    IpV4Multicast {
        ip: Ipv4Addr,
        protocol: TransportProtocol,
        port: u16,
    },
    IpV6Multicast {
        ip: Ipv6Addr,
        protocol: TransportProtocol,
        port: u16,
    },
    IpV4SD {
        ip: Ipv4Addr,
        protocol: TransportProtocol,
        port: u16,
    },
    IpV6SD {
        ip: Ipv6Addr,
        protocol: TransportProtocol,
        port: u16,
    },
}

impl Options {
    #[must_use]
    pub fn size(&self) -> usize {
        match self {
            Options::Configuration {
                configuration_string,
            } => 4 + configuration_string.len(),
            Options::LoadBalancing { .. } => 8,
            Options::IpV4Endpoint { .. }
            | Options::IpV4Multicast { .. }
            | Options::IpV4SD { .. } => 12,
            Options::IpV6Endpoint { .. }
            | Options::IpV6Multicast { .. }
            | Options::IpV6SD { .. } => 24,
        }
    }

    pub fn write<T: embedded_io::Write>(&self, writer: &mut T) -> Result<usize, Error> {
        writer.write_u16_be(u16::try_from(self.size() - 3).expect("option size fits u16"))?;
        match self {
            Options::Configuration {
                configuration_string,
            } => {
                writer.write_u8(u8::from(OptionType::Configuration))?;
                writer.write_u8(0)?;
                writer.write_bytes(configuration_string)?;
                Ok(self.size())
            }
            Options::LoadBalancing { priority, weight } => {
                writer.write_u8(u8::from(OptionType::LoadBalancing))?;
                writer.write_u8(0)?;
                writer.write_u16_be(*priority)?;
                writer.write_u16_be(*weight)?;
                Ok(8)
            }
            Options::IpV4Endpoint { ip, protocol, port } => {
                write_ipv4_option(writer, OptionType::IpV4Endpoint, *ip, *protocol, *port)
            }
            Options::IpV6Endpoint { ip, protocol, port } => {
                write_ipv6_option(writer, OptionType::IpV6Endpoint, *ip, *protocol, *port)
            }
            Options::IpV4Multicast { ip, protocol, port } => {
                write_ipv4_option(writer, OptionType::IpV4Multicast, *ip, *protocol, *port)
            }
            Options::IpV6Multicast { ip, protocol, port } => {
                write_ipv6_option(writer, OptionType::IpV6Multicast, *ip, *protocol, *port)
            }
            Options::IpV4SD { ip, protocol, port } => {
                write_ipv4_option(writer, OptionType::IpV4SD, *ip, *protocol, *port)
            }
            Options::IpV6SD { ip, protocol, port } => {
                write_ipv6_option(writer, OptionType::IpV6SD, *ip, *protocol, *port)
            }
        }
    }

    pub fn read<T: embedded_io::Read>(message_bytes: &mut T) -> Result<Self, Error> {
        let length = message_bytes.read_u16_be()?;
        let option_type = OptionType::try_from(message_bytes.read_u8()?)?;
        let _discard_flag = message_bytes.read_u8()? & 0x80 != 0;

        match option_type {
            OptionType::Configuration => read_configuration(message_bytes, length),
            OptionType::LoadBalancing => read_load_balancing(message_bytes, length),
            OptionType::IpV4Endpoint => {
                validate_length(OptionType::IpV4Endpoint, 9, length)?;
                let (ip, protocol, port) = read_ipv4_fields(message_bytes)?;
                Ok(Options::IpV4Endpoint { ip, protocol, port })
            }
            OptionType::IpV6Endpoint => {
                validate_length(OptionType::IpV6Endpoint, 21, length)?;
                let (ip, protocol, port) = read_ipv6_fields(message_bytes)?;
                Ok(Options::IpV6Endpoint { ip, protocol, port })
            }
            OptionType::IpV4Multicast => {
                validate_length(OptionType::IpV4Multicast, 9, length)?;
                let (ip, protocol, port) = read_ipv4_fields(message_bytes)?;
                Ok(Options::IpV4Multicast { ip, protocol, port })
            }
            OptionType::IpV6Multicast => {
                validate_length(OptionType::IpV6Multicast, 21, length)?;
                let (ip, protocol, port) = read_ipv6_fields(message_bytes)?;
                Ok(Options::IpV6Multicast { ip, protocol, port })
            }
            OptionType::IpV4SD => {
                validate_length(OptionType::IpV4SD, 9, length)?;
                let (ip, protocol, port) = read_ipv4_fields(message_bytes)?;
                Ok(Options::IpV4SD { ip, protocol, port })
            }
            OptionType::IpV6SD => {
                validate_length(OptionType::IpV6SD, 21, length)?;
                let (ip, protocol, port) = read_ipv6_fields(message_bytes)?;
                Ok(Options::IpV6SD { ip, protocol, port })
            }
        }
    }
}

fn validate_length(option_type: OptionType, expected: u16, actual: u16) -> Result<(), Error> {
    if actual != expected {
        return Err(Error::InvalidSDOptionLength {
            option_type: u8::from(option_type),
            expected,
            actual,
        });
    }
    Ok(())
}

fn write_ipv4_option<T: embedded_io::Write>(
    writer: &mut T,
    option_type: OptionType,
    ip: Ipv4Addr,
    protocol: TransportProtocol,
    port: u16,
) -> Result<usize, Error> {
    writer.write_u8(u8::from(option_type))?;
    writer.write_u8(0)?;
    writer.write_u32_be(ip.to_bits())?;
    writer.write_u8(0)?;
    writer.write_u8(u8::from(protocol))?;
    writer.write_u16_be(port)?;
    Ok(12)
}

fn write_ipv6_option<T: embedded_io::Write>(
    writer: &mut T,
    option_type: OptionType,
    ip: Ipv6Addr,
    protocol: TransportProtocol,
    port: u16,
) -> Result<usize, Error> {
    writer.write_u8(u8::from(option_type))?;
    writer.write_u8(0)?;
    writer.write_bytes(&ip.octets())?;
    writer.write_u8(0)?;
    writer.write_u8(u8::from(protocol))?;
    writer.write_u16_be(port)?;
    Ok(24)
}

fn read_ipv4_fields<T: embedded_io::Read>(
    reader: &mut T,
) -> Result<(Ipv4Addr, TransportProtocol, u16), Error> {
    let ip = Ipv4Addr::from_bits(reader.read_u32_be()?);
    let _reserved = reader.read_u8()?;
    let protocol = TransportProtocol::try_from(reader.read_u8()?)?;
    let port = reader.read_u16_be()?;
    Ok((ip, protocol, port))
}

fn read_ipv6_fields<T: embedded_io::Read>(
    reader: &mut T,
) -> Result<(Ipv6Addr, TransportProtocol, u16), Error> {
    let mut octets = [0u8; 16];
    reader.read_bytes(&mut octets)?;
    let ip = Ipv6Addr::from(octets);
    let _reserved = reader.read_u8()?;
    let protocol = TransportProtocol::try_from(reader.read_u8()?)?;
    let port = reader.read_u16_be()?;
    Ok((ip, protocol, port))
}

fn read_configuration<T: embedded_io::Read>(reader: &mut T, length: u16) -> Result<Options, Error> {
    let string_len = length.saturating_sub(1);
    if usize::from(string_len) > MAX_CONFIGURATION_STRING_LENGTH {
        return Err(Error::ConfigurationStringTooLong(string_len.into()));
    }
    let mut buf = [0u8; MAX_CONFIGURATION_STRING_LENGTH];
    let slice = &mut buf[..usize::from(string_len)];
    reader.read_bytes(slice)?;
    let mut configuration_string = heapless::Vec::<u8, MAX_CONFIGURATION_STRING_LENGTH>::new();
    // Length already validated to fit within MAX_CONFIGURATION_STRING_LENGTH
    configuration_string
        .extend_from_slice(slice)
        .expect("length validated above");
    Ok(Options::Configuration {
        configuration_string,
    })
}

fn read_load_balancing<T: embedded_io::Read>(
    reader: &mut T,
    length: u16,
) -> Result<Options, Error> {
    validate_length(OptionType::LoadBalancing, 5, length)?;
    let priority = reader.read_u16_be()?;
    let weight = reader.read_u16_be()?;
    Ok(Options::LoadBalancing { priority, weight })
}

#[cfg(test)]
mod tests {
    use core::net::{Ipv4Addr, Ipv6Addr};

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

    // --- Round-trip tests for all option types ---

    fn round_trip(option: &Options) {
        let size = option.size();
        let mut buf = [0u8; 4 + MAX_CONFIGURATION_STRING_LENGTH];
        let written = option.write(&mut &mut buf[..size]).unwrap();
        assert_eq!(written, size);
        let parsed = Options::read(&mut &buf[..size]).unwrap();
        assert_eq!(*option, parsed);
    }

    #[test]
    fn configuration_round_trip() {
        let mut config_string = heapless::Vec::<u8, MAX_CONFIGURATION_STRING_LENGTH>::new();
        config_string.extend_from_slice(b"test=value").unwrap();
        let option = Options::Configuration {
            configuration_string: config_string,
        };
        round_trip(&option);
    }

    #[test]
    fn configuration_empty_round_trip() {
        let option = Options::Configuration {
            configuration_string: heapless::Vec::new(),
        };
        round_trip(&option);
    }

    #[test]
    fn load_balancing_round_trip() {
        let option = Options::LoadBalancing {
            priority: 100,
            weight: 200,
        };
        round_trip(&option);
    }

    #[test]
    fn ipv4_endpoint_round_trip() {
        let option = Options::IpV4Endpoint {
            ip: Ipv4Addr::new(10, 0, 0, 1),
            protocol: TransportProtocol::Udp,
            port: 30490,
        };
        round_trip(&option);
    }

    #[test]
    fn ipv6_endpoint_round_trip() {
        let option = Options::IpV6Endpoint {
            ip: Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1),
            protocol: TransportProtocol::Tcp,
            port: 8080,
        };
        round_trip(&option);
    }

    #[test]
    fn ipv4_multicast_round_trip() {
        let option = Options::IpV4Multicast {
            ip: Ipv4Addr::new(239, 0, 0, 1),
            protocol: TransportProtocol::Udp,
            port: 30490,
        };
        round_trip(&option);
    }

    #[test]
    fn ipv6_multicast_round_trip() {
        let option = Options::IpV6Multicast {
            ip: Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1),
            protocol: TransportProtocol::Udp,
            port: 30490,
        };
        round_trip(&option);
    }

    #[test]
    fn ipv4_sd_round_trip() {
        let option = Options::IpV4SD {
            ip: Ipv4Addr::new(172, 16, 0, 1),
            protocol: TransportProtocol::Udp,
            port: 30490,
        };
        round_trip(&option);
    }

    #[test]
    fn ipv6_sd_round_trip() {
        let option = Options::IpV6SD {
            ip: Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1),
            protocol: TransportProtocol::Tcp,
            port: 9999,
        };
        round_trip(&option);
    }

    // --- Error cases ---

    #[test]
    fn load_balancing_invalid_length_returns_error() {
        let buf: [u8; 4] = [
            0x00, 0x03, // length = 3 (wrong, should be 5)
            0x02, // type = LoadBalancing
            0x00, // discard flag
        ];
        assert!(matches!(
            Options::read(&mut &buf[..]),
            Err(Error::InvalidSDOptionLength {
                option_type: 0x02,
                expected: 5,
                actual: 3,
            })
        ));
    }

    #[test]
    fn ipv4_endpoint_invalid_length_returns_error() {
        let buf: [u8; 4] = [
            0x00, 0x05, // length = 5 (wrong, should be 9)
            0x04, // type = IpV4Endpoint
            0x00,
        ];
        assert!(matches!(
            Options::read(&mut &buf[..]),
            Err(Error::InvalidSDOptionLength {
                option_type: 0x04,
                expected: 9,
                actual: 5,
            })
        ));
    }

    #[test]
    fn ipv6_endpoint_invalid_length_returns_error() {
        let buf: [u8; 4] = [
            0x00, 0x09, // length = 9 (wrong, should be 21)
            0x06, // type = IpV6Endpoint
            0x00,
        ];
        assert!(matches!(
            Options::read(&mut &buf[..]),
            Err(Error::InvalidSDOptionLength {
                option_type: 0x06,
                expected: 21,
                actual: 9,
            })
        ));
    }

    #[test]
    fn ipv4_multicast_invalid_length_returns_error() {
        let buf: [u8; 4] = [
            0x00, 0x05, // wrong length
            0x14, // type = IpV4Multicast
            0x00,
        ];
        assert!(matches!(
            Options::read(&mut &buf[..]),
            Err(Error::InvalidSDOptionLength {
                option_type: 0x14,
                expected: 9,
                actual: 5,
            })
        ));
    }

    #[test]
    fn ipv6_multicast_invalid_length_returns_error() {
        let buf: [u8; 4] = [
            0x00, 0x09, // wrong length
            0x16, // type = IpV6Multicast
            0x00,
        ];
        assert!(matches!(
            Options::read(&mut &buf[..]),
            Err(Error::InvalidSDOptionLength {
                option_type: 0x16,
                expected: 21,
                actual: 9,
            })
        ));
    }

    #[test]
    fn ipv4_sd_invalid_length_returns_error() {
        let buf: [u8; 4] = [
            0x00, 0x05, // wrong length
            0x24, // type = IpV4SD
            0x00,
        ];
        assert!(matches!(
            Options::read(&mut &buf[..]),
            Err(Error::InvalidSDOptionLength {
                option_type: 0x24,
                expected: 9,
                actual: 5,
            })
        ));
    }

    #[test]
    fn ipv6_sd_invalid_length_returns_error() {
        let buf: [u8; 4] = [
            0x00, 0x09, // wrong length
            0x26, // type = IpV6SD
            0x00,
        ];
        assert!(matches!(
            Options::read(&mut &buf[..]),
            Err(Error::InvalidSDOptionLength {
                option_type: 0x26,
                expected: 21,
                actual: 9,
            })
        ));
    }
}
