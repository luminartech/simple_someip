//! SOME/IP Server/Provider functionality
//!
//! This module provides server-side SOME/IP functionality including:
//! - Service offering/announcement via Service Discovery
//! - Event publishing to subscribers
//! - Event group management
//! - Request/Response handling

mod service_info;
mod event_publisher;
mod subscription_manager;

pub use service_info::{ServiceInfo, EventGroupInfo};
pub use event_publisher::EventPublisher;
pub use subscription_manager::SubscriptionManager;

use crate::protocol::sd::{self, Entry, Flags, OptionsCount, ServiceEntry, TransportProtocol};
use crate::Error;
use std::net::{IpAddr, Ipv4Addr, SocketAddrV4};
use tokio::net::UdpSocket;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use tokio::sync::RwLock;

/// Configuration for a SOME/IP service provider
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Local interface IP address
    pub interface: Ipv4Addr,
    /// Port to bind for receiving subscriptions and requests
    pub local_port: u16,
    /// Service ID being offered
    pub service_id: u16,
    /// Instance ID
    pub instance_id: u16,
    /// Major version
    pub major_version: u8,
    /// Minor version
    pub minor_version: u32,
    /// Service Discovery TTL (time to live)
    pub ttl: u32,
}

impl ServerConfig {
    /// Create a new server configuration
    pub fn new(
        interface: Ipv4Addr,
        local_port: u16,
        service_id: u16,
        instance_id: u16,
    ) -> Self {
        Self {
            interface,
            local_port,
            service_id,
            instance_id,
            major_version: 1,
            minor_version: 0,
            ttl: 3, // 3 seconds is typical for SOME/IP
        }
    }
}

/// SOME/IP Server that can offer services and publish events
pub struct Server {
    config: ServerConfig,
    /// Socket for receiving subscription requests
    unicast_socket: Arc<UdpSocket>,
    /// Socket for sending SD announcements
    sd_socket: Arc<UdpSocket>,
    /// Subscription manager
    subscriptions: Arc<RwLock<SubscriptionManager>>,
    /// Event publisher
    publisher: Arc<EventPublisher>,
    /// Incrementing session ID for SD messages
    sd_session_id: Arc<AtomicU16>,
}

impl Server {
    /// Create a new SOME/IP server
    pub async fn new(config: ServerConfig) -> Result<Self, Error> {
        // Bind unicast socket for receiving subscriptions
        let unicast_addr = SocketAddrV4::new(config.interface, config.local_port);
        let unicast_socket = Arc::new(UdpSocket::bind(unicast_addr).await?);
        tracing::info!("Server bound to {} for service 0x{:04X}", unicast_addr, config.service_id);

        // Bind SD socket for sending/receiving SD messages (must use SD port 30490)
        let expected_sd_port = crate::SD_MULTICAST_PORT;
        let sd_bind_addr = std::net::SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), expected_sd_port);
        let sd_raw_socket = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        sd_raw_socket.set_reuse_address(true)?;
        sd_raw_socket.bind(&sd_bind_addr.into())?;
        sd_raw_socket.set_nonblocking(true)?;
        let sd_std_socket: std::net::UdpSocket = sd_raw_socket.into();
        let sd_socket = UdpSocket::from_std(sd_std_socket)?;

        // Join SD multicast group to receive FindService and SubscribeEventGroup
        sd_socket.join_multicast_v4(crate::SD_MULTICAST_IP, config.interface)?;
        let actual_sd_addr = sd_socket.local_addr()?;
        tracing::info!(
            "Server SD socket bound to {} (expected port {}), joined multicast {}",
            actual_sd_addr, expected_sd_port, crate::SD_MULTICAST_IP
        );
        if let std::net::SocketAddr::V4(v4) = actual_sd_addr {
            if v4.port() != expected_sd_port {
                tracing::error!(
                    "SD socket port mismatch! Expected {}, got {}. Offers will use wrong source port.",
                    expected_sd_port, v4.port()
                );
            }
        }

        let subscriptions = Arc::new(RwLock::new(SubscriptionManager::new()));
        let publisher = Arc::new(EventPublisher::new(
            Arc::clone(&subscriptions),
            Arc::clone(&unicast_socket),
        ));

        Ok(Self {
            config,
            unicast_socket,
            sd_socket: Arc::new(sd_socket),
            subscriptions,
            publisher,
            sd_session_id: Arc::new(AtomicU16::new(1)),
        })
    }

    /// Start announcing the service via Service Discovery
    ///
    /// This sends periodic OfferService messages to the SD multicast group
    pub async fn start_announcing(&self) -> Result<(), Error> {
        let config = self.config.clone();
        let sd_socket = Arc::clone(&self.sd_socket);
        let sd_session_id = Arc::clone(&self.sd_session_id);

        tokio::spawn(async move {
            let mut announcement_count = 0u32;
            loop {
                match Self::send_offer_service(&config, &sd_socket, &sd_session_id).await {
                    Ok(_) => {
                        announcement_count += 1;
                        if announcement_count == 1 {
                            tracing::info!(
                                "Sent first SD announcement for service 0x{:04X}",
                                config.service_id
                            );
                        } else {
                            tracing::debug!(
                                "Sent {} SD announcements for service 0x{:04X}",
                                announcement_count,
                                config.service_id
                            );
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to send OfferService: {:?}", e);
                    }
                }

                // Send announcements every 1 second
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            }
        });

        Ok(())
    }

    /// Send an OfferService message via Service Discovery
    async fn send_offer_service(config: &ServerConfig, socket: &UdpSocket, session_id: &AtomicU16) -> Result<(), Error> {
        use crate::protocol::{Header as SomeIpHeader, MessageId, MessageType, MessageTypeField, ReturnCode};
        use crate::traits::WireFormat;

        // Create OfferService entry
        let entry = Entry::OfferService(ServiceEntry {
            index_first_options_run: 0,
            index_second_options_run: 0,
            options_count: OptionsCount::new(1, 0),
            service_id: config.service_id,
            instance_id: config.instance_id,
            major_version: config.major_version,
            ttl: config.ttl,
            minor_version: config.minor_version,
        });

        // Create IPv4 endpoint option
        let option = sd::Options::IpV4Endpoint {
            ip: config.interface,
            port: config.local_port,
            protocol: TransportProtocol::Udp,
        };

        // Create SD header with reboot flag set
        let sd_payload = sd::Header {
            flags: Flags::new(true, true),
            entries: vec![entry],
            options: vec![option],
        };

        // Encode SD payload
        let mut sd_data = Vec::new();
        sd_payload.encode(&mut sd_data)?;

        // Increment session ID (wrapping from 0xFFFF back to 0x0001, skipping 0)
        let sid = session_id.fetch_add(1, Ordering::Relaxed);
        let sid = if sid == 0 { session_id.fetch_add(1, Ordering::Relaxed) } else { sid };
        let sid = sid as u32;

        // Wrap in SOME/IP header for SD (service 0xFFFF, method 0x8100)
        let someip_header = SomeIpHeader {
            message_id: MessageId::SD,
            length: (sd_data.len() + 8) as u32, // SD payload + 8 bytes header overhead
            session_id: sid,
            protocol_version: 0x01,
            interface_version: 0x01,
            message_type: MessageTypeField::new(MessageType::Notification, false),
            return_code: ReturnCode::Ok,
        };

        // Encode complete SOME/IP-SD message
        let mut buffer = Vec::new();
        someip_header.encode(&mut buffer)?;
        buffer.extend_from_slice(&sd_data);

        let multicast_addr = SocketAddrV4::new(
            crate::SD_MULTICAST_IP,
            crate::SD_MULTICAST_PORT,
        );

        tracing::trace!(
            "Sending OfferService: service=0x{:04X}, instance={}, port={}, size={} bytes",
            config.service_id,
            config.instance_id,
            config.local_port,
            buffer.len()
        );
        tracing::trace!("OfferService data: {:02X?}", &buffer[..buffer.len().min(64)]);

        socket.send_to(&buffer, multicast_addr).await?;
        tracing::trace!("Sent to {}", multicast_addr);

        Ok(())
    }

    /// Send a unicast OfferService to a specific address (in response to FindService)
    async fn send_unicast_offer(&self, target: std::net::SocketAddr) -> Result<(), Error> {
        use crate::protocol::{Header as SomeIpHeader, MessageId, MessageType, MessageTypeField, ReturnCode};
        use crate::traits::WireFormat;

        let entry = Entry::OfferService(ServiceEntry {
            index_first_options_run: 0,
            index_second_options_run: 0,
            options_count: OptionsCount::new(1, 0),
            service_id: self.config.service_id,
            instance_id: self.config.instance_id,
            major_version: self.config.major_version,
            ttl: self.config.ttl,
            minor_version: self.config.minor_version,
        });

        let option = sd::Options::IpV4Endpoint {
            ip: self.config.interface,
            port: self.config.local_port,
            protocol: TransportProtocol::Udp,
        };

        let sd_payload = sd::Header {
            flags: Flags::new(true, true), // reboot + unicast flags set
            entries: vec![entry],
            options: vec![option],
        };

        let mut sd_data = Vec::new();
        sd_payload.encode(&mut sd_data)?;

        let sid = self.next_sd_session_id();
        let someip_header = SomeIpHeader {
            message_id: MessageId::SD,
            length: (sd_data.len() + 8) as u32,
            session_id: sid,
            protocol_version: 0x01,
            interface_version: 0x01,
            message_type: MessageTypeField::new(MessageType::Notification, false),
            return_code: ReturnCode::Ok,
        };

        let mut buffer = Vec::new();
        someip_header.encode(&mut buffer)?;
        buffer.extend_from_slice(&sd_data);

        self.sd_socket.send_to(&buffer, target).await?;
        tracing::debug!(
            "Sent unicast OfferService to {} for service 0x{:04X}",
            target, self.config.service_id
        );

        Ok(())
    }

    /// Get the next SD session ID (client_id=0, session_id incrementing), skipping 0
    fn next_sd_session_id(&self) -> u32 {
        let sid = self.sd_session_id.fetch_add(1, Ordering::Relaxed);
        let sid = if sid == 0 { self.sd_session_id.fetch_add(1, Ordering::Relaxed) } else { sid };
        sid as u32 // client_id (upper 16) = 0, session_id (lower 16) = sid
    }

    /// Get the event publisher for sending events
    pub fn publisher(&self) -> Arc<EventPublisher> {
        Arc::clone(&self.publisher)
    }

    /// Run the server event loop
    ///
    /// Handles incoming subscription requests and manages event groups.
    /// Listens on both the unicast socket (for direct requests) and the
    /// SD multicast socket (for FindService and SubscribeEventGroup).
    pub async fn run(&mut self) -> Result<(), Error> {
        use crate::protocol::Header as SomeIpHeader;
        use crate::traits::WireFormat;
        use std::io::Cursor;

        let mut unicast_buf = vec![0u8; 65535];
        let mut sd_buf = vec![0u8; 65535];

        loop {
            let (data, len, addr, source) = tokio::select! {
                result = self.unicast_socket.recv_from(&mut unicast_buf) => {
                    let (len, addr) = result?;
                    (&unicast_buf[..], len, addr, "unicast")
                }
                result = self.sd_socket.recv_from(&mut sd_buf) => {
                    let (len, addr) = result?;
                    (&sd_buf[..], len, addr, "sd-multicast")
                }
            };
            let data = &data[..len];

            // Skip our own multicast messages
            if let std::net::SocketAddr::V4(v4) = addr {
                if *v4.ip() == self.config.interface && source == "sd-multicast" {
                    tracing::trace!("Ignoring our own SD multicast message");
                    continue;
                }
            }

            tracing::trace!("Received {} bytes from {} on {} socket", len, addr, source);
            tracing::trace!("Raw data: {:02X?}", &data[..len.min(64)]);

            // Try to parse as SOME/IP message
            let mut cursor = Cursor::new(data);
            match SomeIpHeader::decode(&mut cursor) {
                Ok(header) => {
                    tracing::trace!("SOME/IP Header: service=0x{:04X}, method=0x{:04X}, type={:?}",
                        header.message_id.service_id(),
                        header.message_id.method_id(),
                        header.message_type.message_type()
                    );

                    // Check if this is a Service Discovery message (0xFFFF8100)
                    if header.message_id.service_id() == 0xFFFF &&
                       header.message_id.method_id() == 0x8100 {
                        tracing::trace!("This is an SD message");
                        // Parse SD payload
                        match sd::Header::decode(&mut cursor) {
                            Ok(sd_msg) => {
                                tracing::trace!("SD message has {} entries, {} options",
                                    sd_msg.entries.len(),
                                    sd_msg.options.len()
                                );
                                self.handle_sd_message(sd_msg, addr).await?;
                            }
                            Err(e) => {
                                tracing::warn!("Failed to parse SD message: {:?}", e);
                            }
                        }
                    } else {
                        tracing::trace!("Non-SD SOME/IP message, ignoring");
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to parse SOME/IP header from {}: {:?}", addr, e);
                    tracing::trace!("Data: {:02X?}", &data[..len.min(32)]);
                }
            }
        }
    }

    /// Handle a Service Discovery message
    async fn handle_sd_message(
        &mut self,
        sd_msg: sd::Header,
        sender: std::net::SocketAddr,
    ) -> Result<(), Error> {
        tracing::trace!("Handling SD message from {}", sender);

        for entry in &sd_msg.entries {
            match entry {
                Entry::SubscribeEventGroup(sub) => {
                    tracing::debug!(
                        "Received Subscribe from {}: service=0x{:04X}, instance={}, eventgroup=0x{:04X}",
                        sender,
                        sub.service_id,
                        sub.instance_id,
                        sub.event_group_id
                    );

                    // Check if this is for our service
                    if sub.service_id != self.config.service_id {
                        tracing::warn!(
                            "Subscribe for wrong service: expected 0x{:04X}, got 0x{:04X}",
                            self.config.service_id,
                            sub.service_id
                        );
                        self.send_subscribe_nack(sub, sender, "Wrong service ID").await?;
                    } else if sub.instance_id != self.config.instance_id {
                        tracing::warn!(
                            "Subscribe for wrong instance: expected {}, got {}",
                            self.config.instance_id,
                            sub.instance_id
                        );
                        self.send_subscribe_nack(sub, sender, "Wrong instance ID").await?;
                    } else {
                        // Extract subscriber endpoint from options
                        match self.extract_endpoint(&sd_msg.options) {
                            Some(endpoint_addr) => {
                                // The endpoint in SubscribeEventGroup is the subscriber's
                                // receive address — where they want events sent to.
                                let mut subs = self.subscriptions.write().await;
                                subs.subscribe(
                                    sub.service_id,
                                    sub.instance_id,
                                    sub.event_group_id,
                                    endpoint_addr,
                                );

                                // Send SubscribeAck
                                self.send_subscribe_ack(sub, sender).await?;
                            }
                            None => {
                                tracing::warn!("No endpoint found in Subscribe message options");
                                self.send_subscribe_nack(sub, sender, "No endpoint in options").await?;
                            }
                        }
                    }
                }
                Entry::FindService(find) => {
                    // Check if this FindService is for our service (or wildcard 0xFFFF)
                    if find.service_id == self.config.service_id || find.service_id == 0xFFFF {
                        tracing::debug!(
                            "Received FindService from {} for service 0x{:04X} (ours: 0x{:04X}), sending unicast offer",
                            sender, find.service_id, self.config.service_id
                        );
                        self.send_unicast_offer(sender).await?;
                    } else {
                        tracing::trace!(
                            "Ignoring FindService for service 0x{:04X} (not ours)",
                            find.service_id
                        );
                    }
                }
                _ => {
                    tracing::trace!("Ignoring SD entry: {:?}", entry);
                }
            }
        }

        Ok(())
    }

    /// Extract endpoint address from SD options
    fn extract_endpoint(&self, options: &[sd::Options]) -> Option<SocketAddrV4> {
        tracing::trace!("Extracting endpoint from {} options", options.len());
        for option in options {
            tracing::trace!("Option: {:?}", option);
            if let sd::Options::IpV4Endpoint { ip, port, .. } = option {
                tracing::trace!("Found IPv4 endpoint: {}:{}", ip, port);
                return Some(SocketAddrV4::new(*ip, *port));
            }
        }
        tracing::warn!("No IPv4 endpoint found in options");
        None
    }

    /// Send SubscribeAck in response to a subscription request
    async fn send_subscribe_ack(
        &self,
        subscription: &sd::EventGroupEntry,
        subscriber: std::net::SocketAddr,
    ) -> Result<(), Error> {
        use crate::protocol::{Header as SomeIpHeader, MessageId, MessageType, MessageTypeField, ReturnCode};
        use crate::traits::WireFormat;

        // Create SubscribeAck entry
        let ack_entry = Entry::SubscribeAckEventGroup(sd::EventGroupEntry {
            index_first_options_run: 0,
            index_second_options_run: 0,
            options_count: OptionsCount::new(0, 0),
            service_id: subscription.service_id,
            instance_id: subscription.instance_id,
            major_version: subscription.major_version,
            ttl: self.config.ttl,
            counter: subscription.counter,
            event_group_id: subscription.event_group_id,
        });

        // Create SD header
        let sd_payload = sd::Header {
            flags: Flags::new(true, true), // reboot + unicast flags set
            entries: vec![ack_entry],
            options: vec![],
        };

        // Encode SD payload
        let mut sd_data = Vec::new();
        sd_payload.encode(&mut sd_data)?;

        // Wrap in SOME/IP header
        let sid = self.next_sd_session_id();
        let someip_header = SomeIpHeader {
            message_id: MessageId::SD,
            length: (sd_data.len() + 8) as u32,
            session_id: sid,
            protocol_version: 0x01,
            interface_version: 0x01,
            message_type: MessageTypeField::new(MessageType::Notification, false),
            return_code: ReturnCode::Ok,
        };

        // Encode complete message
        let mut buffer = Vec::new();
        someip_header.encode(&mut buffer)?;
        buffer.extend_from_slice(&sd_data);

        // Send SubscribeAck to the subscriber
        self.unicast_socket.send_to(&buffer, subscriber).await?;

        tracing::debug!(
            "Sent SubscribeAck to {} for service 0x{:04X}, eventgroup 0x{:04X}",
            subscriber,
            subscription.service_id,
            subscription.event_group_id
        );

        Ok(())
    }

    /// Send SubscribeNack (Negative Acknowledgement) for rejected subscription
    ///
    /// According to SOME/IP spec, SubscribeNack is indicated by TTL=0 in SubscribeAckEventGroup
    async fn send_subscribe_nack(
        &self,
        subscription: &sd::EventGroupEntry,
        subscriber: std::net::SocketAddr,
        reason: &str,
    ) -> Result<(), Error> {
        use crate::protocol::{Header as SomeIpHeader, MessageId, MessageType, MessageTypeField, ReturnCode};
        use crate::traits::WireFormat;

        // Create SubscribeNack entry (SubscribeAck with TTL=0 indicates rejection)
        let nack_entry = Entry::SubscribeAckEventGroup(sd::EventGroupEntry {
            index_first_options_run: 0,
            index_second_options_run: 0,
            options_count: OptionsCount::new(0, 0),
            service_id: subscription.service_id,
            instance_id: subscription.instance_id,
            major_version: subscription.major_version,
            ttl: 0, // TTL=0 indicates NACK
            counter: subscription.counter,
            event_group_id: subscription.event_group_id,
        });

        // Create SD header
        let sd_payload = sd::Header {
            flags: Flags::new(true, true), // reboot + unicast flags set
            entries: vec![nack_entry],
            options: vec![],
        };

        // Encode SD payload
        let mut sd_data = Vec::new();
        sd_payload.encode(&mut sd_data)?;

        // Wrap in SOME/IP header
        let sid = self.next_sd_session_id();
        let someip_header = SomeIpHeader {
            message_id: MessageId::SD,
            length: (sd_data.len() + 8) as u32,
            session_id: sid,
            protocol_version: 0x01,
            interface_version: 0x01,
            message_type: MessageTypeField::new(MessageType::Notification, false),
            return_code: ReturnCode::Ok,
        };

        // Encode complete message
        let mut buffer = Vec::new();
        someip_header.encode(&mut buffer)?;
        buffer.extend_from_slice(&sd_data);

        // Send SubscribeNack to the subscriber
        self.unicast_socket.send_to(&buffer, subscriber).await?;

        tracing::warn!(
            "Sent SubscribeNack to {} for service 0x{:04X}, eventgroup 0x{:04X} (reason: {})",
            subscriber,
            subscription.service_id,
            subscription.event_group_id,
            reason
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Header as SomeIpHeader, MessageId, MessageType, MessageTypeField, ReturnCode};
    use crate::traits::WireFormat;

    #[tokio::test]
    async fn test_server_creation() {
        let config = ServerConfig::new(
            Ipv4Addr::new(127, 0, 0, 1),
            30682,
            0x5B,
            1,
        );

        let server = Server::new(config).await;
        assert!(server.is_ok());
    }

    /// Helper: wrap an SD header in a SOME/IP SD message and return the bytes
    fn build_sd_message(sd_header: &sd::Header) -> Vec<u8> {
        let mut sd_data = Vec::new();
        sd_header.encode(&mut sd_data).unwrap();

        let someip_header = SomeIpHeader {
            message_id: MessageId::SD,
            length: (sd_data.len() + 8) as u32,
            session_id: 0x0001,
            protocol_version: 0x01,
            interface_version: 0x01,
            message_type: MessageTypeField::new(MessageType::Notification, false),
            return_code: ReturnCode::Ok,
        };

        let mut buffer = Vec::new();
        someip_header.encode(&mut buffer).unwrap();
        buffer.extend_from_slice(&sd_data);
        buffer
    }

    /// Helper: parse a SubscribeAck/Nack from raw response bytes, returns the TTL
    fn parse_subscribe_ack_ttl(data: &[u8]) -> u32 {
        let mut cursor = std::io::Cursor::new(data);
        let _header = SomeIpHeader::decode(&mut cursor).expect("Failed to parse SOME/IP header");
        let sd_msg = sd::Header::decode(&mut cursor).expect("Failed to parse SD header");
        assert_eq!(sd_msg.entries.len(), 1, "Expected exactly 1 entry in response");
        match &sd_msg.entries[0] {
            sd::Entry::SubscribeAckEventGroup(entry) => entry.ttl,
            other => panic!("Expected SubscribeAckEventGroup, got {:?}", other),
        }
    }

    /// Helper: create a server on an ephemeral port and return (Server, port)
    async fn create_test_server(service_id: u16, instance_id: u16) -> (Server, u16) {
        // Use port 0 to get an ephemeral port
        let config = ServerConfig::new(
            Ipv4Addr::new(127, 0, 0, 1),
            0,
            service_id,
            instance_id,
        );
        let mut server = Server::new(config).await.expect("Failed to create server");
        let local_addr = server.unicast_socket.local_addr().unwrap();
        let port = match local_addr {
            std::net::SocketAddr::V4(addr) => addr.port(),
            _ => panic!("Expected IPv4 address"),
        };
        // Update config to reflect actual bound port
        server.config.local_port = port;
        (server, port)
    }

    #[tokio::test]
    async fn test_subscribe_ack_success() {
        let (mut server, server_port) = create_test_server(0x5B, 1).await;

        // Create a client socket to send subscription and receive response
        let client_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let _client_addr = client_socket.local_addr().unwrap();

        // Build a SubscribeEventGroup message with the correct port
        let sd_header = sd::Header::new_subscription(
            0x5B,
            1,
            1,
            3,
            0x01,
            Ipv4Addr::new(127, 0, 0, 1),
            sd::TransportProtocol::Udp,
            server_port, // Correct port
        );
        let message = build_sd_message(&sd_header);

        // Send to the server
        client_socket
            .send_to(&message, format!("127.0.0.1:{}", server_port))
            .await
            .unwrap();

        // Run server to process one message (with a timeout)
        let server_handle = tokio::spawn(async move {
            // We'll manually process one iteration instead of calling run() which loops forever
            let mut buf = vec![0u8; 65535];
            let (len, addr) = server.unicast_socket.recv_from(&mut buf).await.unwrap();
            let data = &buf[..len];
            let mut cursor = std::io::Cursor::new(data);
            let header = SomeIpHeader::decode(&mut cursor).unwrap();
            assert_eq!(header.message_id.service_id(), 0xFFFF);
            let sd_msg = sd::Header::decode(&mut cursor).unwrap();
            server.handle_sd_message(sd_msg, addr).await.unwrap();

            // Check subscription was added
            let subs = server.subscriptions.read().await;
            assert_eq!(subs.subscription_count(), 1);
            let subscribers = subs.get_subscribers(0x5B, 1, 0x01);
            assert_eq!(subscribers.len(), 1);
        });

        // Receive the ACK response
        let mut resp_buf = vec![0u8; 65535];
        let (resp_len, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client_socket.recv_from(&mut resp_buf),
        )
        .await
        .expect("Timeout waiting for SubscribeAck")
        .unwrap();

        let ttl = parse_subscribe_ack_ttl(&resp_buf[..resp_len]);
        assert!(ttl > 0, "Expected ACK (TTL > 0), got TTL={}", ttl);

        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_subscribe_nack_wrong_service() {
        let (mut server, server_port) = create_test_server(0x5B, 1).await;
        let client_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        // Subscribe with wrong service ID (0x99 instead of 0x5B)
        let sd_header = sd::Header::new_subscription(
            0x99, // Wrong service
            1,
            1,
            3,
            0x01,
            Ipv4Addr::new(127, 0, 0, 1),
            sd::TransportProtocol::Udp,
            server_port,
        );
        let message = build_sd_message(&sd_header);
        client_socket
            .send_to(&message, format!("127.0.0.1:{}", server_port))
            .await
            .unwrap();

        // Process the message
        let server_handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let (len, addr) = server.unicast_socket.recv_from(&mut buf).await.unwrap();
            let mut cursor = std::io::Cursor::new(&buf[..len]);
            let _header = SomeIpHeader::decode(&mut cursor).unwrap();
            let sd_msg = sd::Header::decode(&mut cursor).unwrap();
            server.handle_sd_message(sd_msg, addr).await.unwrap();

            // No subscription should have been added
            let subs = server.subscriptions.read().await;
            assert_eq!(subs.subscription_count(), 0);
        });

        // Receive the NACK response
        let mut resp_buf = vec![0u8; 65535];
        let (resp_len, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client_socket.recv_from(&mut resp_buf),
        )
        .await
        .expect("Timeout waiting for SubscribeNack")
        .unwrap();

        let ttl = parse_subscribe_ack_ttl(&resp_buf[..resp_len]);
        assert_eq!(ttl, 0, "Expected NACK (TTL=0), got TTL={}", ttl);

        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_subscribe_nack_wrong_instance() {
        let (mut server, server_port) = create_test_server(0x5B, 1).await;
        let client_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        // Subscribe with wrong instance ID (99 instead of 1)
        let sd_header = sd::Header::new_subscription(
            0x5B,
            99, // Wrong instance
            1,
            3,
            0x01,
            Ipv4Addr::new(127, 0, 0, 1),
            sd::TransportProtocol::Udp,
            server_port,
        );
        let message = build_sd_message(&sd_header);
        client_socket
            .send_to(&message, format!("127.0.0.1:{}", server_port))
            .await
            .unwrap();

        let server_handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let (len, addr) = server.unicast_socket.recv_from(&mut buf).await.unwrap();
            let mut cursor = std::io::Cursor::new(&buf[..len]);
            let _header = SomeIpHeader::decode(&mut cursor).unwrap();
            let sd_msg = sd::Header::decode(&mut cursor).unwrap();
            server.handle_sd_message(sd_msg, addr).await.unwrap();

            let subs = server.subscriptions.read().await;
            assert_eq!(subs.subscription_count(), 0);
        });

        let mut resp_buf = vec![0u8; 65535];
        let (resp_len, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client_socket.recv_from(&mut resp_buf),
        )
        .await
        .expect("Timeout waiting for SubscribeNack")
        .unwrap();

        let ttl = parse_subscribe_ack_ttl(&resp_buf[..resp_len]);
        assert_eq!(ttl, 0, "Expected NACK (TTL=0), got TTL={}", ttl);

        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_subscribe_ack_different_endpoint_port() {
        let (mut server, server_port) = create_test_server(0x5B, 1).await;
        let client_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        // Subscribe with a different endpoint port (subscriber's own receive port)
        // This should succeed — the endpoint port is where the subscriber wants events sent
        let sd_header = sd::Header::new_subscription(
            0x5B,
            1,
            1,
            3,
            0x01,
            Ipv4Addr::new(127, 0, 0, 1),
            sd::TransportProtocol::Udp,
            server_port.wrapping_add(1), // Subscriber's port, different from server
        );
        let message = build_sd_message(&sd_header);
        client_socket
            .send_to(&message, format!("127.0.0.1:{}", server_port))
            .await
            .unwrap();

        let server_handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let (len, addr) = server.unicast_socket.recv_from(&mut buf).await.unwrap();
            let mut cursor = std::io::Cursor::new(&buf[..len]);
            let _header = SomeIpHeader::decode(&mut cursor).unwrap();
            let sd_msg = sd::Header::decode(&mut cursor).unwrap();
            server.handle_sd_message(sd_msg, addr).await.unwrap();

            // Subscription should have been added
            let subs = server.subscriptions.read().await;
            assert_eq!(subs.subscription_count(), 1);
        });

        let mut resp_buf = vec![0u8; 65535];
        let (resp_len, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client_socket.recv_from(&mut resp_buf),
        )
        .await
        .expect("Timeout waiting for SubscribeAck")
        .unwrap();

        let ttl = parse_subscribe_ack_ttl(&resp_buf[..resp_len]);
        assert!(ttl > 0, "Expected ACK (TTL > 0), got TTL={}", ttl);

        server_handle.await.unwrap();
    }
}
