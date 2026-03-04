use core::net::{Ipv4Addr, Ipv6Addr};

use super::Error;
use crate::protocol::byte_order::WriteBytesExt;

/// Maximum length of an SD configuration option string in bytes.
pub const MAX_CONFIGURATION_STRING_LENGTH: usize = 256;

/// Transport protocol used in SD endpoint options.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TransportProtocol {
    /// UDP (0x11).
    Udp,
    /// TCP (0x06).
    Tcp,
}

impl TryFrom<u8> for TransportProtocol {
    type Error = Error;
    fn try_from(value: u8) -> Result<Self, Error> {
        match value {
            0x11 => Ok(TransportProtocol::Udp),
            0x06 => Ok(TransportProtocol::Tcp),
            _ => Err(Error::InvalidOptionTransportProtocol(value)),
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

/// The type of an SD option.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OptionType {
    /// Configuration option (0x01).
    Configuration,
    /// Load balancing option (0x02).
    LoadBalancing,
    /// IPv4 endpoint option (0x04).
    IpV4Endpoint,
    /// IPv6 endpoint option (0x06).
    IpV6Endpoint,
    /// IPv4 multicast option (0x14).
    IpV4Multicast,
    /// IPv6 multicast option (0x16).
    IpV6Multicast,
    /// IPv4 SD option (0x24).
    IpV4SD,
    /// IPv6 SD option (0x26).
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
            _ => Err(Error::InvalidOptionType(value)),
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
/// A decoded SD option.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Options {
    /// A configuration key-value string.
    Configuration {
        /// The raw configuration string bytes.
        configuration_string: heapless::Vec<u8, MAX_CONFIGURATION_STRING_LENGTH>,
    },
    /// Load balancing parameters.
    LoadBalancing {
        /// The priority value.
        priority: u16,
        /// The weight value.
        weight: u16,
    },
    /// An IPv4 endpoint.
    IpV4Endpoint {
        /// The IPv4 address.
        ip: Ipv4Addr,
        /// The transport protocol (UDP or TCP).
        protocol: TransportProtocol,
        /// The port number.
        port: u16,
    },
    /// An IPv6 endpoint.
    IpV6Endpoint {
        /// The IPv6 address.
        ip: Ipv6Addr,
        /// The transport protocol (UDP or TCP).
        protocol: TransportProtocol,
        /// The port number.
        port: u16,
    },
    /// An IPv4 multicast address.
    IpV4Multicast {
        /// The IPv4 multicast address.
        ip: Ipv4Addr,
        /// The transport protocol (UDP or TCP).
        protocol: TransportProtocol,
        /// The port number.
        port: u16,
    },
    /// An IPv6 multicast address.
    IpV6Multicast {
        /// The IPv6 multicast address.
        ip: Ipv6Addr,
        /// The transport protocol (UDP or TCP).
        protocol: TransportProtocol,
        /// The port number.
        port: u16,
    },
    /// An IPv4 SD endpoint.
    IpV4SD {
        /// The IPv4 address.
        ip: Ipv4Addr,
        /// The transport protocol (UDP or TCP).
        protocol: TransportProtocol,
        /// The port number.
        port: u16,
    },
    /// An IPv6 SD endpoint.
    IpV6SD {
        /// The IPv6 address.
        ip: Ipv6Addr,
        /// The transport protocol (UDP or TCP).
        protocol: TransportProtocol,
        /// The port number.
        port: u16,
    },
}

impl Options {
    /// Returns the total wire size of this option in bytes.
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

    /// Serializes this option to a writer.
    ///
    /// # Errors
    ///
    /// Returns an error if writing to the writer fails.
    ///
    /// # Panics
    ///
    /// Panics if the option size minus 3 exceeds `u16::MAX` (unreachable in practice).
    pub fn write<T: embedded_io::Write>(
        &self,
        writer: &mut T,
    ) -> Result<usize, crate::protocol::Error> {
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
}

fn write_ipv4_option<T: embedded_io::Write>(
    writer: &mut T,
    option_type: OptionType,
    ip: Ipv4Addr,
    protocol: TransportProtocol,
    port: u16,
) -> Result<usize, crate::protocol::Error> {
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
) -> Result<usize, crate::protocol::Error> {
    writer.write_u8(u8::from(option_type))?;
    writer.write_u8(0)?;
    writer.write_bytes(&ip.octets())?;
    writer.write_u8(0)?;
    writer.write_u8(u8::from(protocol))?;
    writer.write_u16_be(port)?;
    Ok(24)
}

/// Extract the first `IpV4Endpoint` address from a slice of owned options.
///
/// Returns `None` if no `IpV4Endpoint` option is present.
#[must_use]
pub fn extract_ipv4_endpoint(options: &[Options]) -> Option<core::net::SocketAddrV4> {
    options.iter().find_map(|opt| match opt {
        Options::IpV4Endpoint { ip, port, .. } => Some(core::net::SocketAddrV4::new(*ip, *port)),
        _ => None,
    })
}

// --- Zero-copy view types ---

/// Zero-copy view into a variable-length SD option in a buffer.
///
/// Wire layout:
/// - `[0..2]`: length (u16 BE) = `total_size` - 3
/// - `[2]`: option type (u8)
/// - `[3]`: reserved/discard flag (u8)
/// - `[4..]`: type-specific data
#[derive(Clone, Copy, Debug)]
pub struct OptionView<'a>(&'a [u8]);

impl<'a> OptionView<'a> {
    /// Returns the option type.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidOptionType`] if the type byte is unrecognized.
    pub fn option_type(&self) -> Result<OptionType, Error> {
        OptionType::try_from(self.0[2])
    }

    /// Total wire size of this option (length field value + 3).
    #[must_use]
    pub fn wire_size(&self) -> usize {
        let length = u16::from_be_bytes([self.0[0], self.0[1]]);
        usize::from(length) + 3
    }

    /// Parse as IPv4 endpoint/multicast/SD option.
    /// Returns `(ip, protocol, port)`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidOptionTransportProtocol`] if the protocol byte is unrecognized.
    pub fn as_ipv4(&self) -> Result<(Ipv4Addr, TransportProtocol, u16), Error> {
        let ip = Ipv4Addr::from_bits(u32::from_be_bytes([
            self.0[4], self.0[5], self.0[6], self.0[7],
        ]));
        // [8] is reserved
        let protocol = TransportProtocol::try_from(self.0[9])?;
        let port = u16::from_be_bytes([self.0[10], self.0[11]]);
        Ok((ip, protocol, port))
    }

    /// Parse as IPv6 endpoint/multicast/SD option.
    /// Returns `(ip, protocol, port)`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidOptionTransportProtocol`] if the protocol byte is unrecognized.
    pub fn as_ipv6(&self) -> Result<(Ipv6Addr, TransportProtocol, u16), Error> {
        let mut octets = [0u8; 16];
        octets.copy_from_slice(&self.0[4..20]);
        let ip = Ipv6Addr::from(octets);
        // [20] is reserved
        let protocol = TransportProtocol::try_from(self.0[21])?;
        let port = u16::from_be_bytes([self.0[22], self.0[23]]);
        Ok((ip, protocol, port))
    }

    /// Raw configuration bytes (for Configuration options).
    #[must_use]
    pub fn configuration_bytes(&self) -> &'a [u8] {
        let length = u16::from_be_bytes([self.0[0], self.0[1]]);
        let string_len = length.saturating_sub(1);
        &self.0[4..4 + usize::from(string_len)]
    }

    /// Parse as load-balancing option. Returns `(priority, weight)`.
    ///
    /// # Errors
    ///
    /// Currently always succeeds; the `Result` return type is reserved for future validation.
    pub fn as_load_balancing(&self) -> Result<(u16, u16), Error> {
        let priority = u16::from_be_bytes([self.0[4], self.0[5]]);
        let weight = u16::from_be_bytes([self.0[6], self.0[7]]);
        Ok((priority, weight))
    }

    /// Converts this view into an owned [`Options`].
    ///
    /// # Errors
    ///
    /// Returns an error if the option type is unrecognized, the transport protocol byte
    /// is invalid, or the configuration string exceeds [`MAX_CONFIGURATION_STRING_LENGTH`].
    ///
    /// # Panics
    ///
    /// Panics if a configuration string passes the length check but fails to fit into the
    /// heapless buffer (unreachable in practice).
    pub fn to_owned(&self) -> Result<Options, Error> {
        let option_type = self.option_type()?;
        match option_type {
            OptionType::Configuration => {
                let config_bytes = self.configuration_bytes();
                if config_bytes.len() > MAX_CONFIGURATION_STRING_LENGTH {
                    return Err(Error::ConfigurationStringTooLong(config_bytes.len()));
                }
                let mut configuration_string =
                    heapless::Vec::<u8, MAX_CONFIGURATION_STRING_LENGTH>::new();
                configuration_string
                    .extend_from_slice(config_bytes)
                    .expect("length validated above");
                Ok(Options::Configuration {
                    configuration_string,
                })
            }
            OptionType::LoadBalancing => {
                let (priority, weight) = self.as_load_balancing()?;
                Ok(Options::LoadBalancing { priority, weight })
            }
            OptionType::IpV4Endpoint => {
                let (ip, protocol, port) = self.as_ipv4()?;
                Ok(Options::IpV4Endpoint { ip, protocol, port })
            }
            OptionType::IpV6Endpoint => {
                let (ip, protocol, port) = self.as_ipv6()?;
                Ok(Options::IpV6Endpoint { ip, protocol, port })
            }
            OptionType::IpV4Multicast => {
                let (ip, protocol, port) = self.as_ipv4()?;
                Ok(Options::IpV4Multicast { ip, protocol, port })
            }
            OptionType::IpV6Multicast => {
                let (ip, protocol, port) = self.as_ipv6()?;
                Ok(Options::IpV6Multicast { ip, protocol, port })
            }
            OptionType::IpV4SD => {
                let (ip, protocol, port) = self.as_ipv4()?;
                Ok(Options::IpV4SD { ip, protocol, port })
            }
            OptionType::IpV6SD => {
                let (ip, protocol, port) = self.as_ipv6()?;
                Ok(Options::IpV6SD { ip, protocol, port })
            }
        }
    }
}

/// Iterator over variable-length SD options in a validated buffer.
/// Options are guaranteed valid (validated upfront in `SdHeaderView::parse`).
pub struct OptionIter<'a> {
    remaining: &'a [u8],
}

impl<'a> OptionIter<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { remaining: buf }
    }
}

impl<'a> Iterator for OptionIter<'a> {
    type Item = OptionView<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.len() < 4 {
            return None;
        }
        let length = u16::from_be_bytes([self.remaining[0], self.remaining[1]]);
        let wire_size = usize::from(length) + 3;
        if wire_size > self.remaining.len() {
            return None;
        }
        let view = OptionView(&self.remaining[..wire_size]);
        self.remaining = &self.remaining[wire_size..];
        Some(view)
    }
}

/// Validate a single option's wire format and return its wire size.
/// Used during `SdHeaderView::parse` for upfront validation.
pub(crate) fn validate_option(buf: &[u8]) -> Result<usize, Error> {
    if buf.len() < 4 {
        return Err(Error::IncorrectOptionsSize(buf.len()));
    }
    let length = u16::from_be_bytes([buf[0], buf[1]]);
    let wire_size = usize::from(length) + 3;
    if wire_size > buf.len() {
        return Err(Error::IncorrectOptionsSize(buf.len()));
    }
    let option_type = OptionType::try_from(buf[2])?;
    // Validate expected lengths for fixed-size options
    match option_type {
        OptionType::IpV4Endpoint | OptionType::IpV4Multicast | OptionType::IpV4SD => {
            if length != 9 {
                return Err(Error::InvalidOptionLength {
                    option_type: buf[2],
                    expected: 9,
                    actual: length,
                });
            }
        }
        OptionType::IpV6Endpoint | OptionType::IpV6Multicast | OptionType::IpV6SD => {
            if length != 21 {
                return Err(Error::InvalidOptionLength {
                    option_type: buf[2],
                    expected: 21,
                    actual: length,
                });
            }
        }
        OptionType::LoadBalancing => {
            if length != 5 {
                return Err(Error::InvalidOptionLength {
                    option_type: buf[2],
                    expected: 5,
                    actual: length,
                });
            }
        }
        OptionType::Configuration => {
            // Configuration strings are variable length; just check it doesn't exceed max
            let string_len = length.saturating_sub(1);
            if usize::from(string_len) > MAX_CONFIGURATION_STRING_LENGTH {
                return Err(Error::ConfigurationStringTooLong(string_len.into()));
            }
        }
    }
    Ok(wire_size)
}

#[cfg(test)]
mod tests {
    use core::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

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
            Err(Error::InvalidOptionTransportProtocol(0xFF))
        ));
    }

    // --- OptionView: parse from encoded bytes ---

    #[test]
    fn option_view_ipv4_endpoint_tcp() {
        let buf: [u8; 12] = [
            0x00, 0x09, // length = 9
            0x04, // type = IpV4Endpoint
            0x00, // discard flag
            192, 168, 0, 1,    // ip
            0x00, // reserved
            0x06, // protocol = TCP
            0x04, 0xD2, // port = 1234
        ];
        let view = OptionView(&buf);
        assert_eq!(view.option_type().unwrap(), OptionType::IpV4Endpoint);
        assert_eq!(view.wire_size(), 12);
        let (ip, protocol, port) = view.as_ipv4().unwrap();
        assert_eq!(ip, Ipv4Addr::new(192, 168, 0, 1));
        assert_eq!(protocol, TransportProtocol::Tcp);
        assert_eq!(port, 1234);
    }

    #[test]
    fn option_view_to_owned_invalid_type() {
        let buf: [u8; 4] = [0x00, 0x00, 0xFF, 0x00]; // type = 0xFF (invalid)
        let view = OptionView(&buf);
        assert!(matches!(
            view.to_owned(),
            Err(Error::InvalidOptionType(0xFF))
        ));
    }

    // --- Round-trip tests for all option types ---

    fn round_trip(option: &Options) {
        let size = option.size();
        let mut buf = [0u8; 4 + MAX_CONFIGURATION_STRING_LENGTH];
        let written = option.write(&mut &mut buf[..size]).unwrap();
        assert_eq!(written, size);
        let view = OptionView(&buf[..size]);
        let parsed = view.to_owned().unwrap();
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
        // length = 3 (wrong, should be 5), wire_size = 6
        let mut buf = [0u8; 6];
        buf[0] = 0x00;
        buf[1] = 0x03; // length = 3
        buf[2] = 0x02; // type = LoadBalancing
        buf[3] = 0x00; // discard flag
        assert!(matches!(
            validate_option(&buf),
            Err(Error::InvalidOptionLength {
                option_type: 0x02,
                expected: 5,
                actual: 3,
            })
        ));
    }

    #[test]
    fn ipv4_endpoint_invalid_length_returns_error() {
        // length = 5 (wrong, should be 9), wire_size = 8
        let mut buf = [0u8; 8];
        buf[0] = 0x00;
        buf[1] = 0x05; // length = 5
        buf[2] = 0x04; // type = IpV4Endpoint
        buf[3] = 0x00;
        assert!(matches!(
            validate_option(&buf),
            Err(Error::InvalidOptionLength {
                option_type: 0x04,
                expected: 9,
                actual: 5,
            })
        ));
    }

    #[test]
    fn ipv6_endpoint_invalid_length_returns_error() {
        // length = 9 (wrong, should be 21), wire_size = 12
        let mut buf = [0u8; 12];
        buf[0] = 0x00;
        buf[1] = 0x09; // length = 9
        buf[2] = 0x06; // type = IpV6Endpoint
        buf[3] = 0x00;
        assert!(matches!(
            validate_option(&buf),
            Err(Error::InvalidOptionLength {
                option_type: 0x06,
                expected: 21,
                actual: 9,
            })
        ));
    }

    #[test]
    fn ipv4_multicast_invalid_length_returns_error() {
        // length = 5 (wrong, should be 9), wire_size = 8
        let mut buf = [0u8; 8];
        buf[0] = 0x00;
        buf[1] = 0x05;
        buf[2] = 0x14; // type = IpV4Multicast
        buf[3] = 0x00;
        assert!(matches!(
            validate_option(&buf),
            Err(Error::InvalidOptionLength {
                option_type: 0x14,
                expected: 9,
                actual: 5,
            })
        ));
    }

    #[test]
    fn ipv6_multicast_invalid_length_returns_error() {
        // length = 9 (wrong, should be 21), wire_size = 12
        let mut buf = [0u8; 12];
        buf[0] = 0x00;
        buf[1] = 0x09;
        buf[2] = 0x16; // type = IpV6Multicast
        buf[3] = 0x00;
        assert!(matches!(
            validate_option(&buf),
            Err(Error::InvalidOptionLength {
                option_type: 0x16,
                expected: 21,
                actual: 9,
            })
        ));
    }

    #[test]
    fn ipv4_sd_invalid_length_returns_error() {
        // length = 5 (wrong, should be 9), wire_size = 8
        let mut buf = [0u8; 8];
        buf[0] = 0x00;
        buf[1] = 0x05;
        buf[2] = 0x24; // type = IpV4SD
        buf[3] = 0x00;
        assert!(matches!(
            validate_option(&buf),
            Err(Error::InvalidOptionLength {
                option_type: 0x24,
                expected: 9,
                actual: 5,
            })
        ));
    }

    #[test]
    fn ipv6_sd_invalid_length_returns_error() {
        // length = 9 (wrong, should be 21), wire_size = 12
        let mut buf = [0u8; 12];
        buf[0] = 0x00;
        buf[1] = 0x09;
        buf[2] = 0x26; // type = IpV6SD
        buf[3] = 0x00;
        assert!(matches!(
            validate_option(&buf),
            Err(Error::InvalidOptionLength {
                option_type: 0x26,
                expected: 21,
                actual: 9,
            })
        ));
    }

    // --- OptionIter ---

    #[test]
    fn option_iter_empty() {
        let iter = OptionIter::new(&[]);
        assert_eq!(iter.count(), 0);
    }

    #[test]
    fn option_iter_two_options() {
        let opt1 = Options::IpV4Endpoint {
            ip: Ipv4Addr::new(10, 0, 0, 1),
            protocol: TransportProtocol::Udp,
            port: 30490,
        };
        let opt2 = Options::LoadBalancing {
            priority: 100,
            weight: 200,
        };
        let mut buf = [0u8; 24]; // 12 + 8 = 20
        let n1 = opt1.write(&mut &mut buf[..12]).unwrap();
        let n2 = opt2.write(&mut &mut buf[12..20]).unwrap();
        let total = n1 + n2;

        let mut iter = OptionIter::new(&buf[..total]);
        let v1 = iter.next().unwrap();
        assert_eq!(v1.to_owned().unwrap(), opt1);
        let v2 = iter.next().unwrap();
        assert_eq!(v2.to_owned().unwrap(), opt2);
        assert!(iter.next().is_none());
    }
}
