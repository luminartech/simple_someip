//! SOME/IP Server/Provider functionality
//!
//! This module provides server-side SOME/IP functionality including:
//! - Service offering/announcement via Service Discovery
//! - Event publishing to subscribers
//! - Event group management
//! - Request/Response handling

mod error;
mod event_publisher;
mod service_info;
mod subscription_manager;

pub use error::Error;
pub use event_publisher::EventPublisher;
pub use service_info::{EventGroupInfo, ServiceInfo};
pub use subscription_manager::SubscriptionManager;

use crate::protocol::sd::{self, Entry, Flags, OptionsCount, ServiceEntry, TransportProtocol};
use core::sync::atomic::Ordering;
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddrV4},
    sync::{Arc, atomic::AtomicU16},
    vec,
    vec::Vec,
};
use tokio::{net::UdpSocket, sync::RwLock};

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
    #[must_use]
    pub fn new(interface: Ipv4Addr, local_port: u16, service_id: u16, instance_id: u16) -> Self {
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
    ///
    /// # Errors
    ///
    /// Returns an error if binding the unicast or SD socket fails, or if joining the
    /// SD multicast group fails.
    pub async fn new(config: ServerConfig) -> Result<Self, Error> {
        // Bind unicast socket for receiving subscriptions
        let unicast_addr = SocketAddrV4::new(config.interface, config.local_port);
        let unicast_socket = Arc::new(UdpSocket::bind(unicast_addr).await?);
        tracing::info!(
            "Server bound to {} for service 0x{:04X}",
            unicast_addr,
            config.service_id
        );

        // Bind SD socket for sending/receiving SD messages (must use SD port 30490)
        let expected_sd_port = sd::MULTICAST_PORT;
        let sd_bind_addr =
            std::net::SocketAddr::new(IpAddr::V4(config.interface), expected_sd_port);
        let sd_raw_socket = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        sd_raw_socket.set_reuse_address(true)?;
        sd_raw_socket.set_multicast_if_v4(&config.interface)?;
        sd_raw_socket.set_multicast_loop_v4(false)?;
        sd_raw_socket.bind(&sd_bind_addr.into())?;
        sd_raw_socket.set_nonblocking(true)?;
        let sd_std_socket: std::net::UdpSocket = sd_raw_socket.into();
        let sd_socket = UdpSocket::from_std(sd_std_socket)?;

        // Join SD multicast group to receive FindService and SubscribeEventGroup
        sd_socket.join_multicast_v4(sd::MULTICAST_IP, config.interface)?;
        let actual_sd_addr = sd_socket.local_addr()?;
        tracing::info!(
            "Server SD socket bound to {} (expected port {}), joined multicast {}",
            actual_sd_addr,
            expected_sd_port,
            sd::MULTICAST_IP
        );
        if let std::net::SocketAddr::V4(v4) = actual_sd_addr
            && v4.port() != expected_sd_port
        {
            tracing::error!(
                "SD socket port mismatch! Expected {}, got {}. Offers will use wrong source port.",
                expected_sd_port,
                v4.port()
            );
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
    /// This sends periodic `OfferService` messages to the SD multicast group
    ///
    /// # Errors
    ///
    /// Currently always returns `Ok(())`; SD send failures are logged internally.
    pub fn start_announcing(&self) -> Result<(), Error> {
        let config = self.config.clone();
        let sd_socket = Arc::clone(&self.sd_socket);
        let sd_session_id = Arc::clone(&self.sd_session_id);

        tokio::spawn(async move {
            let mut announcement_count = 0u32;
            loop {
                match Self::send_offer_service(&config, &sd_socket, &sd_session_id).await {
                    Ok(()) => {
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

    /// Send an `OfferService` message via Service Discovery
    async fn send_offer_service(
        config: &ServerConfig,
        socket: &UdpSocket,
        session_id: &AtomicU16,
    ) -> Result<(), Error> {
        use crate::protocol::Header as SomeIpHeader;
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

        let entries = [entry];
        let options = [option];
        let sd_payload = sd::Header::new(Flags::new(true, true), &entries, &options);

        // Encode SD payload
        let mut sd_data = Vec::new();
        sd_payload.encode(&mut sd_data)?;

        // Increment session ID (wrapping from 0xFFFF back to 0x0001, skipping 0)
        let prev = session_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                let next = v.wrapping_add(1);
                Some(if next == 0 { 1 } else { next })
            })
            .unwrap();
        let next = prev.wrapping_add(1);
        let sid = u32::from(if next == 0 { 1 } else { next });

        // Wrap in SOME/IP header for SD (service 0xFFFF, method 0x8100)
        let someip_header = SomeIpHeader::new_sd(sid, sd_data.len());

        // Encode complete SOME/IP-SD message
        let mut buffer = Vec::new();
        someip_header.encode(&mut buffer)?;
        buffer.extend_from_slice(&sd_data);

        let multicast_addr = SocketAddrV4::new(sd::MULTICAST_IP, sd::MULTICAST_PORT);

        tracing::trace!(
            "Sending OfferService: service=0x{:04X}, instance={}, port={}, size={} bytes",
            config.service_id,
            config.instance_id,
            config.local_port,
            buffer.len()
        );
        tracing::trace!(
            "OfferService data: {:02X?}",
            &buffer[..buffer.len().min(64)]
        );

        socket.send_to(&buffer, multicast_addr).await?;
        tracing::trace!("Sent to {}", multicast_addr);

        Ok(())
    }

    /// Send a unicast `OfferService` to a specific address (in response to `FindService`)
    async fn send_unicast_offer(&self, target: std::net::SocketAddr) -> Result<(), Error> {
        use crate::protocol::Header as SomeIpHeader;
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

        let entries = [entry];
        let options = [option];
        let sd_payload = sd::Header::new(Flags::new(true, true), &entries, &options);

        let mut sd_data = Vec::new();
        sd_payload.encode(&mut sd_data)?;

        let sid = self.next_sd_session_id();
        let someip_header = SomeIpHeader::new_sd(sid, sd_data.len());

        let mut buffer = Vec::new();
        someip_header.encode(&mut buffer)?;
        buffer.extend_from_slice(&sd_data);

        self.sd_socket.send_to(&buffer, target).await?;
        tracing::debug!(
            "Sent unicast OfferService to {} for service 0x{:04X}",
            target,
            self.config.service_id
        );

        Ok(())
    }

    /// Get the next SD session ID (`client_id=0`, `session_id` incrementing), skipping 0
    fn next_sd_session_id(&self) -> u32 {
        let prev = self
            .sd_session_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                let next = v.wrapping_add(1);
                Some(if next == 0 { 1 } else { next })
            })
            .unwrap();
        // fetch_update returns the previous value; compute the same next value
        let next = prev.wrapping_add(1);
        u32::from(if next == 0 { 1 } else { next })
    }

    /// Get the event publisher for sending events
    #[must_use]
    pub fn publisher(&self) -> Arc<EventPublisher> {
        Arc::clone(&self.publisher)
    }

    /// Get the local address of the unicast socket.
    ///
    /// # Errors
    ///
    /// Returns an error if the socket's local address cannot be retrieved.
    pub fn unicast_local_addr(&self) -> Result<std::net::SocketAddr, std::io::Error> {
        self.unicast_socket.local_addr()
    }

    /// Update the configured local port (useful after binding to ephemeral port 0).
    pub fn set_local_port(&mut self, port: u16) {
        self.config.local_port = port;
    }

    /// Run the server event loop
    ///
    /// Handles incoming subscription requests and manages event groups.
    /// Listens on both the unicast socket (for direct requests) and the
    /// SD multicast socket (for `FindService` and `SubscribeEventGroup`).
    ///
    /// # Errors
    ///
    /// Returns an error if receiving from a socket fails or handling an SD message fails.
    pub async fn run(&mut self) -> Result<(), Error> {
        use crate::protocol::MessageView;

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

            // Own multicast messages are suppressed via IP_MULTICAST_LOOP=false
            // on the SD socket, so no source-IP filtering is needed here.

            tracing::trace!("Received {} bytes from {} on {} socket", len, addr, source);
            tracing::trace!("Raw data: {:02X?}", &data[..len.min(64_usize)]);

            // Try to parse as SOME/IP message using zero-copy view
            match MessageView::parse(data) {
                Ok(view) => {
                    tracing::trace!(
                        "SOME/IP Header: service=0x{:04X}, method=0x{:04X}, type={:?}",
                        view.header().message_id().service_id(),
                        view.header().message_id().method_id(),
                        view.header().message_type().message_type()
                    );

                    // Check if this is a Service Discovery message (0xFFFF8100)
                    if view.is_sd() {
                        tracing::trace!("This is an SD message");
                        // Parse SD payload
                        match view.sd_header() {
                            Ok(sd_view) => {
                                tracing::trace!("SD message has {} entries", sd_view.entry_count(),);
                                self.handle_sd_message(&sd_view, addr).await?;
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
        sd_view: &sd::SdHeaderView<'_>,
        sender: std::net::SocketAddr,
    ) -> Result<(), Error> {
        tracing::trace!("Handling SD message from {}", sender);

        for entry_view in sd_view.entries() {
            let entry_type = entry_view.entry_type()?;
            match entry_type {
                sd::EntryType::Subscribe => {
                    tracing::debug!(
                        "Received Subscribe from {}: service=0x{:04X}, instance={}, eventgroup=0x{:04X}",
                        sender,
                        entry_view.service_id(),
                        entry_view.instance_id(),
                        entry_view.event_group_id()
                    );

                    // Check if this is for our service
                    if entry_view.service_id() != self.config.service_id {
                        tracing::warn!(
                            "Subscribe for wrong service: expected 0x{:04X}, got 0x{:04X}",
                            self.config.service_id,
                            entry_view.service_id()
                        );
                        self.send_subscribe_nack_from_view(&entry_view, sender, "Wrong service ID")
                            .await?;
                    } else if entry_view.instance_id() != self.config.instance_id {
                        tracing::warn!(
                            "Subscribe for wrong instance: expected {}, got {}",
                            self.config.instance_id,
                            entry_view.instance_id()
                        );
                        self.send_subscribe_nack_from_view(
                            &entry_view,
                            sender,
                            "Wrong instance ID",
                        )
                        .await?;
                    } else {
                        // Extract subscriber endpoint from options
                        if let Some(endpoint_addr) =
                            Self::extract_endpoint_from_views(sd_view.options())
                        {
                            let mut subs = self.subscriptions.write().await;
                            subs.subscribe(
                                entry_view.service_id(),
                                entry_view.instance_id(),
                                entry_view.event_group_id(),
                                endpoint_addr,
                            );

                            // Send SubscribeAck
                            self.send_subscribe_ack_from_view(&entry_view, sender)
                                .await?;
                        } else {
                            tracing::warn!("No endpoint found in Subscribe message options");
                            self.send_subscribe_nack_from_view(
                                &entry_view,
                                sender,
                                "No endpoint in options",
                            )
                            .await?;
                        }
                    }
                }
                sd::EntryType::FindService => {
                    let find_service_id = entry_view.service_id();
                    // Check if this FindService is for our service (or wildcard 0xFFFF)
                    if find_service_id == self.config.service_id || find_service_id == 0xFFFF {
                        tracing::debug!(
                            "Received FindService from {} for service 0x{:04X} (ours: 0x{:04X}), sending unicast offer",
                            sender,
                            find_service_id,
                            self.config.service_id
                        );
                        self.send_unicast_offer(sender).await?;
                    } else {
                        tracing::trace!(
                            "Ignoring FindService for service 0x{:04X} (not ours)",
                            find_service_id
                        );
                    }
                }
                _ => {
                    tracing::trace!("Ignoring SD entry type: {:?}", entry_type);
                }
            }
        }

        Ok(())
    }

    /// Extract endpoint address from SD option views
    fn extract_endpoint_from_views(options: sd::OptionIter<'_>) -> Option<SocketAddrV4> {
        for option_view in options {
            if let Ok(sd::OptionType::IpV4Endpoint) = option_view.option_type()
                && let Ok((ip, _, port)) = option_view.as_ipv4()
            {
                tracing::trace!("Found IPv4 endpoint: {}:{}", ip, port);
                return Some(SocketAddrV4::new(ip, port));
            }
        }
        tracing::warn!("No IPv4 endpoint found in options");
        None
    }

    /// Send `SubscribeAck` from an entry view
    async fn send_subscribe_ack_from_view(
        &self,
        entry_view: &sd::EntryView<'_>,
        subscriber: std::net::SocketAddr,
    ) -> Result<(), Error> {
        use crate::protocol::Header as SomeIpHeader;
        use crate::traits::WireFormat;

        let ack_entry = Entry::SubscribeAckEventGroup(sd::EventGroupEntry {
            index_first_options_run: 0,
            index_second_options_run: 0,
            options_count: OptionsCount::new(0, 0),
            service_id: entry_view.service_id(),
            instance_id: entry_view.instance_id(),
            major_version: entry_view.major_version(),
            ttl: self.config.ttl,
            counter: entry_view.counter(),
            event_group_id: entry_view.event_group_id(),
        });

        let entries = [ack_entry];
        let sd_payload = sd::Header::new(Flags::new(true, true), &entries, &[]);

        let mut sd_data = Vec::new();
        sd_payload.encode(&mut sd_data)?;

        let sid = self.next_sd_session_id();
        let someip_header = SomeIpHeader::new_sd(sid, sd_data.len());

        let mut buffer = Vec::new();
        someip_header.encode(&mut buffer)?;
        buffer.extend_from_slice(&sd_data);

        self.unicast_socket.send_to(&buffer, subscriber).await?;

        tracing::debug!(
            "Sent SubscribeAck to {} for service 0x{:04X}, eventgroup 0x{:04X}",
            subscriber,
            entry_view.service_id(),
            entry_view.event_group_id()
        );

        Ok(())
    }

    /// Send `SubscribeNack` from an entry view
    async fn send_subscribe_nack_from_view(
        &self,
        entry_view: &sd::EntryView<'_>,
        subscriber: std::net::SocketAddr,
        reason: &str,
    ) -> Result<(), Error> {
        use crate::protocol::Header as SomeIpHeader;
        use crate::traits::WireFormat;

        let nack_entry = Entry::SubscribeAckEventGroup(sd::EventGroupEntry {
            index_first_options_run: 0,
            index_second_options_run: 0,
            options_count: OptionsCount::new(0, 0),
            service_id: entry_view.service_id(),
            instance_id: entry_view.instance_id(),
            major_version: entry_view.major_version(),
            ttl: 0, // TTL=0 indicates NACK
            counter: entry_view.counter(),
            event_group_id: entry_view.event_group_id(),
        });

        let entries = [nack_entry];
        let sd_payload = sd::Header::new(Flags::new(true, true), &entries, &[]);

        let mut sd_data = Vec::new();
        sd_payload.encode(&mut sd_data)?;

        let sid = self.next_sd_session_id();
        let someip_header = SomeIpHeader::new_sd(sid, sd_data.len());

        let mut buffer = Vec::new();
        someip_header.encode(&mut buffer)?;
        buffer.extend_from_slice(&sd_data);

        self.unicast_socket.send_to(&buffer, subscriber).await?;

        tracing::warn!(
            "Sent SubscribeNack to {} for service 0x{:04X}, eventgroup 0x{:04X} (reason: {})",
            subscriber,
            entry_view.service_id(),
            entry_view.event_group_id(),
            reason
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        Header as SomeIpHeader, MessageType, MessageTypeField, MessageView, ReturnCode,
    };
    use crate::traits::WireFormat;
    use std::format;

    #[tokio::test]
    async fn test_server_creation() {
        let config = ServerConfig::new(Ipv4Addr::new(127, 0, 0, 1), 30682, 0x5B, 1);

        let server: Result<Server, _> = Server::new(config).await;
        assert!(server.is_ok());
    }

    /// Helper: wrap an SD header in a SOME/IP SD message and return the bytes
    fn build_sd_message(sd_header: &sd::Header<'_>) -> Vec<u8> {
        let mut sd_data = Vec::new();
        sd_header.encode(&mut sd_data).unwrap();

        let someip_header = SomeIpHeader::new_sd(0x0001, sd_data.len());

        let mut buffer = Vec::new();
        someip_header.encode(&mut buffer).unwrap();
        buffer.extend_from_slice(&sd_data);
        buffer
    }

    /// Helper: parse a SubscribeAck/Nack from raw response bytes, returns the TTL
    fn parse_subscribe_ack_ttl(data: &[u8]) -> u32 {
        let view = MessageView::parse(data).expect("Failed to parse SOME/IP message");
        let sd_view = view.sd_header().expect("Failed to parse SD header");
        let mut entries = sd_view.entries();
        let entry = entries.next().expect("Expected at least 1 entry");
        assert_eq!(
            entry.entry_type().unwrap(),
            sd::EntryType::SubscribeAck,
            "Expected SubscribeAckEventGroup entry"
        );
        entry.ttl()
    }

    /// Helper: create a server on an ephemeral port and return (Server, port)
    async fn create_test_server(service_id: u16, instance_id: u16) -> (Server, u16) {
        // Use port 0 to get an ephemeral port
        let config = ServerConfig::new(Ipv4Addr::new(127, 0, 0, 1), 0, service_id, instance_id);
        let mut server = Server::new(config).await.expect("Failed to create server");
        let port = match server.unicast_local_addr().unwrap() {
            std::net::SocketAddr::V4(addr) => addr.port(),
            _ => panic!("Expected IPv4 address"),
        };
        // Update config to reflect actual bound port
        server.set_local_port(port);
        (server, port)
    }

    fn make_subscription_header(
        service_id: u16,
        instance_id: u16,
        major_version: u8,
        ttl: u32,
        event_group_id: u16,
        client_ip: Ipv4Addr,
        protocol: sd::TransportProtocol,
        client_port: u16,
    ) -> Vec<u8> {
        let entry = Entry::SubscribeEventGroup(sd::EventGroupEntry::new(
            service_id,
            instance_id,
            major_version,
            ttl,
            event_group_id,
        ));
        let endpoint = sd::Options::IpV4Endpoint {
            ip: client_ip,
            protocol,
            port: client_port,
        };
        let entries = [entry];
        let options = [endpoint];
        let sd_header = sd::Header::new(Flags::new_sd(false), &entries, &options);
        build_sd_message(&sd_header)
    }

    #[tokio::test]
    async fn test_subscribe_ack_success() {
        let (mut server, server_port) = create_test_server(0x5B, 1).await;

        // Create a client socket to send subscription and receive response
        let client_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let message = make_subscription_header(
            0x5B,
            1,
            1,
            3,
            0x01,
            Ipv4Addr::new(127, 0, 0, 1),
            sd::TransportProtocol::Udp,
            server_port,
        );

        // Send to the server
        client_socket
            .send_to(&message, format!("127.0.0.1:{}", server_port))
            .await
            .unwrap();

        // Run server to process one message (with a timeout)
        let server_handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let (len, addr) = server.unicast_socket.recv_from(&mut buf).await.unwrap();
            let data = &buf[..len];
            let view = MessageView::parse(data).unwrap();
            let sd_view = view.sd_header().unwrap();
            server.handle_sd_message(&sd_view, addr).await.unwrap();

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

        let message = make_subscription_header(
            0x99, // Wrong service
            1,
            1,
            3,
            0x01,
            Ipv4Addr::new(127, 0, 0, 1),
            sd::TransportProtocol::Udp,
            server_port,
        );
        client_socket
            .send_to(&message, format!("127.0.0.1:{}", server_port))
            .await
            .unwrap();

        // Process the message
        let server_handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let (len, addr) = server.unicast_socket.recv_from(&mut buf).await.unwrap();
            let data = &buf[..len];
            let view = MessageView::parse(data).unwrap();
            let sd_view = view.sd_header().unwrap();
            server.handle_sd_message(&sd_view, addr).await.unwrap();

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

        let message = make_subscription_header(
            0x5B,
            99, // Wrong instance
            1,
            3,
            0x01,
            Ipv4Addr::new(127, 0, 0, 1),
            sd::TransportProtocol::Udp,
            server_port,
        );
        client_socket
            .send_to(&message, format!("127.0.0.1:{}", server_port))
            .await
            .unwrap();

        let server_handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let (len, addr) = server.unicast_socket.recv_from(&mut buf).await.unwrap();
            let data = &buf[..len];
            let view = MessageView::parse(data).unwrap();
            let sd_view = view.sd_header().unwrap();
            server.handle_sd_message(&sd_view, addr).await.unwrap();

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
    async fn test_find_service_sends_unicast_offer() {
        let (mut server, server_port) = create_test_server(0x5B, 1).await;
        let client_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        // Send a FindService for 0x5B
        let find_entry = Entry::FindService(ServiceEntry::find(0x5B));
        let find_entries = [find_entry];
        let sd_header = sd::Header::new(Flags::new_sd(false), &find_entries, &[]);
        let message = build_sd_message(&sd_header);
        client_socket
            .send_to(&message, format!("127.0.0.1:{}", server_port))
            .await
            .unwrap();

        // Process the message on the unicast socket
        let server_handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let (len, addr) = server.unicast_socket.recv_from(&mut buf).await.unwrap();
            let data = &buf[..len];
            let view = MessageView::parse(data).unwrap();
            let sd_view = view.sd_header().unwrap();
            server.handle_sd_message(&sd_view, addr).await.unwrap();
        });

        // Receive the unicast OfferService response
        let mut resp_buf = vec![0u8; 65535];
        let (resp_len, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client_socket.recv_from(&mut resp_buf),
        )
        .await
        .expect("Timeout waiting for unicast OfferService")
        .unwrap();

        // Parse the response and verify it's an OfferService for 0x5B
        let view = MessageView::parse(&resp_buf[..resp_len]).unwrap();
        assert_eq!(view.header().message_id().service_id(), 0xFFFF);
        let sd_view = view.sd_header().unwrap();
        let mut entries = sd_view.entries();
        let entry = entries.next().unwrap();
        assert_eq!(entry.entry_type().unwrap(), sd::EntryType::OfferService);
        assert_eq!(entry.service_id(), 0x5B);

        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_find_service_wildcard() {
        let (mut server, server_port) = create_test_server(0x5B, 1).await;
        let client_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        // Send wildcard FindService (0xFFFF)
        let find_entry = Entry::FindService(ServiceEntry::find(0xFFFF));
        let find_entries = [find_entry];
        let sd_header = sd::Header::new(Flags::new_sd(false), &find_entries, &[]);
        let message = build_sd_message(&sd_header);
        client_socket
            .send_to(&message, format!("127.0.0.1:{}", server_port))
            .await
            .unwrap();

        let server_handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let (len, addr) = server.unicast_socket.recv_from(&mut buf).await.unwrap();
            let data = &buf[..len];
            let view = MessageView::parse(data).unwrap();
            let sd_view = view.sd_header().unwrap();
            server.handle_sd_message(&sd_view, addr).await.unwrap();
        });

        let mut resp_buf = vec![0u8; 65535];
        let (resp_len, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client_socket.recv_from(&mut resp_buf),
        )
        .await
        .expect("Timeout waiting for unicast OfferService")
        .unwrap();

        let view = MessageView::parse(&resp_buf[..resp_len]).unwrap();
        let sd_view = view.sd_header().unwrap();
        let mut entries = sd_view.entries();
        let entry = entries.next().unwrap();
        assert_eq!(entry.entry_type().unwrap(), sd::EntryType::OfferService);
        assert_eq!(entry.service_id(), 0x5B);

        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_find_service_wrong_service_ignored() {
        let (mut server, server_port) = create_test_server(0x5B, 1).await;
        let client_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        // Send FindService for 0x99 (not our service)
        let find_entry = Entry::FindService(ServiceEntry::find(0x99));
        let find_entries = [find_entry];
        let sd_header = sd::Header::new(Flags::new_sd(false), &find_entries, &[]);
        let message = build_sd_message(&sd_header);
        client_socket
            .send_to(&message, format!("127.0.0.1:{}", server_port))
            .await
            .unwrap();

        let server_handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let (len, addr) = server.unicast_socket.recv_from(&mut buf).await.unwrap();
            let data = &buf[..len];
            let view = MessageView::parse(data).unwrap();
            let sd_view = view.sd_header().unwrap();
            server.handle_sd_message(&sd_view, addr).await.unwrap();
        });

        // Should NOT receive any response (short timeout)
        let mut resp_buf = vec![0u8; 65535];
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            client_socket.recv_from(&mut resp_buf),
        )
        .await;
        assert!(
            result.is_err(),
            "Expected timeout (no response for wrong service)"
        );

        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_subscribe_nack_no_endpoint() {
        let (mut server, server_port) = create_test_server(0x5B, 1).await;
        let client_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        // Build a SubscribeEventGroup with NO endpoint option
        let entry = sd::Entry::SubscribeEventGroup(sd::EventGroupEntry::new(0x5B, 1, 1, 3, 0x01));
        let sub_entries = [entry];
        let sd_header = sd::Header::new(Flags::new(true, true), &sub_entries, &[]);
        let message = build_sd_message(&sd_header);

        client_socket
            .send_to(&message, format!("127.0.0.1:{}", server_port))
            .await
            .unwrap();

        let server_handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let (len, addr) = server.unicast_socket.recv_from(&mut buf).await.unwrap();
            let data = &buf[..len];
            let view = MessageView::parse(data).unwrap();
            let sd_view = view.sd_header().unwrap();
            server.handle_sd_message(&sd_view, addr).await.unwrap();

            // No subscription should have been added
            let subs = server.subscriptions.read().await;
            assert_eq!(subs.subscription_count(), 0);
        });

        // Should receive a NACK (TTL=0)
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
    async fn test_send_offer_service() {
        // Test send_unicast_offer directly (sends to a specific target).
        // send_offer_service sends to multicast which is unreliable on loopback.
        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let recv_addr = receiver.local_addr().unwrap();

        let (server, _) = create_test_server(0x5B, 1).await;
        server
            .send_unicast_offer(recv_addr)
            .await
            .expect("send_unicast_offer failed");

        // Receive and parse the offer
        let mut buf = vec![0u8; 65535];
        let (len, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            receiver.recv_from(&mut buf),
        )
        .await
        .expect("Timeout waiting for OfferService")
        .unwrap();

        let view = MessageView::parse(&buf[..len]).unwrap();
        assert_eq!(view.header().message_id(), crate::protocol::MessageId::SD);
        let sd_view = view.sd_header().unwrap();
        let mut entries = sd_view.entries();
        let entry = entries.next().unwrap();
        assert_eq!(entry.entry_type().unwrap(), sd::EntryType::OfferService);
        assert_eq!(entry.service_id(), 0x5B);
        assert_eq!(entry.instance_id(), 1);

        // Also test that start_announcing doesn't error
        drop(server);
        let (server2, _) = create_test_server(0x5B, 1).await;
        assert!(server2.start_announcing().is_ok());
    }

    #[tokio::test]
    async fn test_run_non_sd_message() {
        let (mut server, server_port) = create_test_server(0x5B, 1).await;
        let client_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_port = match client_socket.local_addr().unwrap() {
            std::net::SocketAddr::V4(a) => a.port(),
            _ => panic!("expected v4"),
        };

        let subscriptions = Arc::clone(&server.subscriptions);

        let server_handle = tokio::spawn(async move {
            server.run().await.ok();
        });

        // Send a non-SD SOME/IP message (service 0x1234, method 0x0001)
        let non_sd_header = SomeIpHeader::new(
            crate::protocol::MessageId::new_from_service_and_method(0x1234, 0x0001),
            0x0001,
            0x01,
            0x01,
            MessageTypeField::new(MessageType::Request, false),
            ReturnCode::Ok,
            0,
        );
        let mut non_sd_buf = Vec::new();
        non_sd_header.encode(&mut non_sd_buf).unwrap();
        client_socket
            .send_to(&non_sd_buf, format!("127.0.0.1:{}", server_port))
            .await
            .unwrap();

        // Small delay, then send valid subscribe
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let message = make_subscription_header(
            0x5B,
            1,
            1,
            3,
            0x01,
            Ipv4Addr::new(127, 0, 0, 1),
            sd::TransportProtocol::Udp,
            client_port,
        );
        client_socket
            .send_to(&message, format!("127.0.0.1:{}", server_port))
            .await
            .unwrap();

        // Wait for ACK
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

        // Verify subscription was added (non-SD message was ignored)
        let subs = subscriptions.read().await;
        assert_eq!(subs.subscription_count(), 1);

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_run_malformed_data() {
        let (mut server, server_port) = create_test_server(0x5B, 1).await;
        let client_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_port = match client_socket.local_addr().unwrap() {
            std::net::SocketAddr::V4(a) => a.port(),
            _ => panic!("expected v4"),
        };

        let subscriptions = Arc::clone(&server.subscriptions);

        let server_handle = tokio::spawn(async move {
            server.run().await.ok();
        });

        // Send garbage bytes
        client_socket
            .send_to(&[0xFF, 0xFE, 0xFD], format!("127.0.0.1:{}", server_port))
            .await
            .unwrap();

        // Small delay, then send valid subscribe
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let message = make_subscription_header(
            0x5B,
            1,
            1,
            3,
            0x01,
            Ipv4Addr::new(127, 0, 0, 1),
            sd::TransportProtocol::Udp,
            client_port,
        );
        client_socket
            .send_to(&message, format!("127.0.0.1:{}", server_port))
            .await
            .unwrap();

        // Wait for ACK
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

        let subs = subscriptions.read().await;
        assert_eq!(subs.subscription_count(), 1);

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_next_sd_session_id_wraps() {
        let (server, _) = create_test_server(0x5B, 1).await;

        // Set session ID to 0xFFFE
        server.sd_session_id.store(0xFFFE, Ordering::Relaxed);

        // First call: 0xFFFE -> 0xFFFF, returns 0xFFFF
        let sid1 = server.next_sd_session_id();
        assert_eq!(sid1, 0xFFFF);

        // Second call: 0xFFFF -> wraps to 0x0001 (skipping 0), returns 0x0001
        let sid2 = server.next_sd_session_id();
        assert_eq!(sid2, 0x0001);
    }

    #[tokio::test]
    async fn test_handle_sd_other_entry_type() {
        let (mut server, _) = create_test_server(0x5B, 1).await;

        // Build SD message with a StopOfferService entry (not handled by server)
        let entry = sd::Entry::StopOfferService(sd::ServiceEntry {
            index_first_options_run: 0,
            index_second_options_run: 0,
            options_count: sd::OptionsCount::new(0, 0),
            service_id: 0x5B,
            instance_id: 1,
            major_version: 1,
            ttl: 0,
            minor_version: 0,
        });
        let stop_entries = [entry];
        let sd_msg = sd::Header::new(Flags::new(true, true), &stop_entries, &[]);

        // Encode and parse through view types
        let mut buf = [0u8; 64];
        let n = sd_msg.encode(&mut buf.as_mut_slice()).unwrap();
        let sd_view = sd::SdHeaderView::parse(&buf[..n]).unwrap();

        // Should not panic or error
        let result = server
            .handle_sd_message(&sd_view, "127.0.0.1:12345".parse().unwrap())
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_subscribe_ack_different_endpoint_port() {
        let (mut server, server_port) = create_test_server(0x5B, 1).await;
        let client_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let message = make_subscription_header(
            0x5B,
            1,
            1,
            3,
            0x01,
            Ipv4Addr::new(127, 0, 0, 1),
            sd::TransportProtocol::Udp,
            server_port.wrapping_add(1), // Subscriber's port, different from server
        );
        client_socket
            .send_to(&message, format!("127.0.0.1:{}", server_port))
            .await
            .unwrap();

        let server_handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            let (len, addr) = server.unicast_socket.recv_from(&mut buf).await.unwrap();
            let data = &buf[..len];
            let view = MessageView::parse(data).unwrap();
            let sd_view = view.sd_header().unwrap();
            server.handle_sd_message(&sd_view, addr).await.unwrap();

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
