//! Service Discovery session-state tracking, decoupled from socket ownership.
//!
//! [`SdStateManager`] owns the session-ID counter used by every outgoing
//! SOME/IP-SD message this server emits (`OfferService` announcements,
//! unicast Offer replies, `SubscribeAck`, `SubscribeNack`). It also builds
//! and sends `OfferService` announcements when given a socket.
//!
//! Keeping this state in its own type prepares the server for upcoming
//! transport abstraction: once `TransportSocket` lands, the `&UdpSocket`
//! parameter on [`SdStateManager::send_offer_service`] becomes the single
//! migration point for the announcement path.

use core::sync::atomic::{AtomicU16, Ordering};
use std::{net::SocketAddrV4, vec::Vec};
use tokio::net::UdpSocket;

use crate::protocol::sd::{self, Entry, Flags, OptionsCount, ServiceEntry, TransportProtocol};

use super::{Error, ServerConfig};

/// Tracks the SD session-ID counter and emits `OfferService` announcements.
///
/// Session IDs increment with each SD message and wrap from `0xFFFF` back
/// to `0x0001` (skipping `0`, which is reserved).
#[derive(Debug)]
pub(super) struct SdStateManager {
    session_id: AtomicU16,
}

impl SdStateManager {
    pub(super) const fn new() -> Self {
        Self {
            session_id: AtomicU16::new(1),
        }
    }

    /// Advance the counter and return the next SOME/IP-SD session ID
    /// (`client_id = 0`, session ID in the low 16 bits). Skips 0 on wrap.
    pub(super) fn next_session_id(&self) -> u32 {
        let prev = self
            .session_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                let next = v.wrapping_add(1);
                Some(if next == 0 { 1 } else { next })
            })
            .unwrap();
        let next = prev.wrapping_add(1);
        u32::from(if next == 0 { 1 } else { next })
    }

    /// Send a multicast `OfferService` announcement for the given config.
    pub(super) async fn send_offer_service(
        &self,
        config: &ServerConfig,
        socket: &UdpSocket,
    ) -> Result<(), Error> {
        use crate::protocol::Header as SomeIpHeader;
        use crate::traits::WireFormat;

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

        let option = sd::Options::IpV4Endpoint {
            ip: config.interface,
            port: config.local_port,
            protocol: TransportProtocol::Udp,
        };

        let entries = [entry];
        let options = [option];
        let sd_payload = sd::Header::new(Flags::new(true, true), &entries, &options);

        let mut sd_data = Vec::new();
        sd_payload.encode(&mut sd_data)?;

        let sid = self.next_session_id();
        let someip_header = SomeIpHeader::new_sd(sid, sd_data.len());

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

    #[cfg(test)]
    pub(super) fn store_for_test(&self, v: u16) {
        self.session_id.store(v, Ordering::Relaxed);
    }
}

impl Default for SdStateManager {
    fn default() -> Self {
        Self::new()
    }
}
