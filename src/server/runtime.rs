//! Server runtime helpers — free async functions that drive the
//! receive loop, the SD announcement loop, and SD-message handling.
//!
//! These live as free functions (rather than `&self` methods on
//! [`Server`]) so the run-future returned from `Server::new` can be
//! `'static` — built by cloning the cheap shared-handles into an
//! `async move` instead of borrowing whatever `Server` value the
//! caller holds.
//!
//! All functions here take their state by reference; ownership lives
//! in the caller's async-move scope, which is itself constructed by
//! [`Server::run`](super::Server::run) /
//! [`Server::run_with_buffers`](super::Server::run_with_buffers).

use crate::CapacityKind;
use core::net::SocketAddrV4;

use futures_util::{FutureExt, future::Either, pin_mut, select_biased};

use crate::Timer;
use crate::protocol::sd::{self, Entry, Flags, OptionsCount, ServiceEntry, TransportProtocol};
use crate::transport::{E2EHandle, SharedHandle, TransportSocket};

use super::sd_state::SdStateManager;
use super::subscription_manager::{SubscribeError, SubscriptionHandle};
use super::{Error, ServerConfig};

/// Send a unicast `OfferService` to a specific address (typically in
/// response to a `FindService`).
///
/// `buf` is a caller-provided scratch buffer used for encoding the outgoing
/// frame. Returns [`Error::Capacity`]`("udp_buffer")` if the encoded frame
/// does not fit in `buf`.
pub(super) async fn send_unicast_offer<T>(
    buf: &mut [u8],
    config: &ServerConfig,
    sd_socket: &T,
    sd_state: &SdStateManager,
    target: core::net::SocketAddr,
) -> Result<(), Error>
where
    T: TransportSocket,
{
    use crate::protocol::Header as SomeIpHeader;
    use automotive_wire_codec::Encode;

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
    let (sid, reboot_flag) = sd_state.next_session_id_with_reboot_flag();
    let sd_payload = sd::Header::new(Flags::new_sd(reboot_flag), &entries, &options);

    // Guard: SOME/IP header needs 16 bytes; SD payload needs the rest.
    if buf.len() < 16 {
        return Err(Error::Capacity(CapacityKind::UdpBuffer));
    }
    let sd_data_len = sd_payload
        .encode_to_slice(&mut buf[16..])
        .map_err(super::sd_encode_error)?;
    let total_len = 16 + sd_data_len;
    // The `< 16` guard plus `encode_to_slice`'s own over-capacity error
    // already cover the fit; this stays as a debug-only sanity check
    // rather than a live (dead) branch.
    debug_assert!(total_len <= buf.len());
    let someip_header = SomeIpHeader::new_sd(sid, sd_data_len);
    someip_header
        .encode_to_slice(&mut buf[..16])
        .map_err(|_| Error::Capacity(CapacityKind::UdpBuffer))?;

    let target_v4 = socket_addr_v4(target)?;
    sd_socket.send_to(&buf[..total_len], target_v4).await?;
    crate::log::debug!(
        "Sent unicast OfferService to {} for service 0x{:04X}",
        target,
        config.service_id
    );

    Ok(())
}

/// Send `SubscribeAck` derived from a peer's `Subscribe` entry view.
///
/// `buf` is a caller-provided scratch buffer used for encoding the outgoing
/// frame. Returns [`Error::Capacity`]`("udp_buffer")` if the encoded frame
/// does not fit in `buf`.
pub(super) async fn send_subscribe_ack_from_view<T>(
    buf: &mut [u8],
    config: &ServerConfig,
    sd_socket: &T,
    sd_state: &SdStateManager,
    entry_view: &sd::EntryView<'_>,
    subscriber: core::net::SocketAddr,
) -> Result<(), Error>
where
    T: TransportSocket,
{
    use crate::protocol::Header as SomeIpHeader;
    use automotive_wire_codec::Encode;

    let ack_entry = Entry::SubscribeAckEventGroup(sd::EventGroupEntry {
        index_first_options_run: 0,
        index_second_options_run: 0,
        options_count: OptionsCount::new(0, 0),
        service_id: entry_view.service_id(),
        instance_id: entry_view.instance_id(),
        major_version: entry_view.major_version(),
        ttl: config.ttl,
        counter: entry_view.counter(),
        event_group_id: entry_view.event_group_id(),
    });

    let entries = [ack_entry];
    let (sid, reboot_flag) = sd_state.next_session_id_with_reboot_flag();
    let sd_payload = sd::Header::new(Flags::new_sd(reboot_flag), &entries, &[]);

    // Guard: SOME/IP header needs 16 bytes; SD payload needs the rest.
    if buf.len() < 16 {
        return Err(Error::Capacity(CapacityKind::UdpBuffer));
    }
    let sd_data_len = sd_payload
        .encode_to_slice(&mut buf[16..])
        .map_err(super::sd_encode_error)?;
    let total_len = 16 + sd_data_len;
    // The `< 16` guard plus `encode_to_slice`'s own over-capacity error
    // already cover the fit; this stays as a debug-only sanity check
    // rather than a live (dead) branch.
    debug_assert!(total_len <= buf.len());
    let someip_header = SomeIpHeader::new_sd(sid, sd_data_len);
    someip_header
        .encode_to_slice(&mut buf[..16])
        .map_err(|_| Error::Capacity(CapacityKind::UdpBuffer))?;

    let subscriber_v4 = socket_addr_v4(subscriber)?;
    sd_socket.send_to(&buf[..total_len], subscriber_v4).await?;

    crate::log::debug!(
        "Sent SubscribeAck to {} for service 0x{:04X}, eventgroup 0x{:04X}",
        subscriber,
        entry_view.service_id(),
        entry_view.event_group_id()
    );

    Ok(())
}

/// Send `SubscribeNack` (`SubscribeAckEventGroup` with `ttl = 0`).
///
/// `buf` is a caller-provided scratch buffer used for encoding the outgoing
/// frame. Returns [`Error::Capacity`]`("udp_buffer")` if the encoded frame
/// does not fit in `buf`.
pub(super) async fn send_subscribe_nack_from_view<T>(
    buf: &mut [u8],
    _config: &ServerConfig,
    sd_socket: &T,
    sd_state: &SdStateManager,
    entry_view: &sd::EntryView<'_>,
    subscriber: core::net::SocketAddr,
    reason: &str,
) -> Result<(), Error>
where
    T: TransportSocket,
{
    use crate::protocol::Header as SomeIpHeader;
    use automotive_wire_codec::Encode;

    let nack_entry = Entry::SubscribeAckEventGroup(sd::EventGroupEntry {
        index_first_options_run: 0,
        index_second_options_run: 0,
        options_count: OptionsCount::new(0, 0),
        service_id: entry_view.service_id(),
        instance_id: entry_view.instance_id(),
        major_version: entry_view.major_version(),
        ttl: 0,
        counter: entry_view.counter(),
        event_group_id: entry_view.event_group_id(),
    });

    let entries = [nack_entry];
    let (sid, reboot_flag) = sd_state.next_session_id_with_reboot_flag();
    let sd_payload = sd::Header::new(Flags::new_sd(reboot_flag), &entries, &[]);

    // Guard: SOME/IP header needs 16 bytes; SD payload needs the rest.
    if buf.len() < 16 {
        return Err(Error::Capacity(CapacityKind::UdpBuffer));
    }
    let sd_data_len = sd_payload
        .encode_to_slice(&mut buf[16..])
        .map_err(super::sd_encode_error)?;
    let total_len = 16 + sd_data_len;
    // The `< 16` guard plus `encode_to_slice`'s own over-capacity error
    // already cover the fit; this stays as a debug-only sanity check
    // rather than a live (dead) branch.
    debug_assert!(total_len <= buf.len());
    let someip_header = SomeIpHeader::new_sd(sid, sd_data_len);
    someip_header
        .encode_to_slice(&mut buf[..16])
        .map_err(|_| Error::Capacity(CapacityKind::UdpBuffer))?;

    let subscriber_v4 = socket_addr_v4(subscriber)?;
    sd_socket.send_to(&buf[..total_len], subscriber_v4).await?;

    crate::log::warn!(
        "Sent SubscribeNack to {} for service 0x{:04X}, eventgroup 0x{:04X} (reason: {})",
        subscriber,
        entry_view.service_id(),
        entry_view.event_group_id(),
        reason
    );

    Ok(())
}

/// Encode into `buf` one SD message carrying a Subscribe entry for each
/// consumed eventgroup marked in `answered`; returns its length.
fn encode_subscribes(
    buf: &mut [u8],
    config: &ServerConfig,
    sd_state: &SdStateManager,
    answered: &[bool],
) -> Result<usize, Error> {
    let mut requests = config.consumed_eventgroups.clone();
    let mut answered = answered.iter();
    requests.retain(|_| answered.next().copied().unwrap_or(false));
    let (sid, reboot_flag) = sd_state.next_session_id_with_reboot_flag();
    #[allow(clippy::cast_possible_truncation)]
    let session = sid as u16;
    crate::sd_codec::build_multi_subscribe_eventgroup_datagram::<
        { ServerConfig::CONSUMED_EVENTGROUPS_CAP },
    >(buf, &requests, session, reboot_flag)
    .map_err(|_| Error::Capacity(CapacityKind::UdpBuffer))
}

/// Send `server` one SD message carrying a Subscribe entry for each
/// consumed eventgroup marked in `answered`.
async fn send_subscribes<T>(
    buf: &mut [u8],
    config: &ServerConfig,
    sd_socket: &T,
    sd_state: &SdStateManager,
    answered: &[bool],
    server: core::net::SocketAddr,
) -> Result<(), Error>
where
    T: TransportSocket,
{
    let len = encode_subscribes(buf, config, sd_state, answered)?;
    sd_socket
        .send_to(&buf[..len], socket_addr_v4(server)?)
        .await?;
    crate::log::debug!("Sent Subscribe to {}", server);
    Ok(())
}

/// Handle a Service Discovery message (Subscribe / `FindService` etc.).
#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_sd_message<T, Sub>(
    config: &ServerConfig,
    sd_socket: &T,
    sd_state: &SdStateManager,
    subscriptions: &Sub,
    sd_view: &sd::SdHeaderView<'_>,
    sender: core::net::SocketAddr,
    send_buf: &mut [u8],
    offered: &[core::sync::atomic::AtomicBool],
) -> Result<(), Error>
where
    T: TransportSocket,
    Sub: SubscriptionHandle,
{
    crate::log::trace!("Handling SD message from {}", sender);

    // `send_buf` is the caller-owned send scratch threaded down from
    // `recv_loop` (which holds exactly one — only one inbound SD message
    // is handled at a time, so only one helper send is ever in flight).
    // It replaces the former future-resident `[u8; UDP_BUFFER_SIZE]`,
    // keeping that buffer out of the run future's frame.

    let mut answered = [false; ServerConfig::CONSUMED_EVENTGROUPS_CAP];
    for (entry_view, entry_options) in sd_view.entries_with_options() {
        let Ok(entry_type) = entry_view.entry_type() else {
            continue;
        };
        match entry_type {
            sd::EntryType::Subscribe => {
                crate::log::debug!(
                    "Received Subscribe from {}: service=0x{:04X}, instance={}, eventgroup=0x{:04X}",
                    sender,
                    entry_view.service_id(),
                    entry_view.instance_id(),
                    entry_view.event_group_id()
                );

                // A co-offered `(service, instance, major_version,
                // event_group)` registered via `with_accepted_offer` is
                // accepted on this shared recv loop even though it is not the
                // primary service — its tuple (major version included) is
                // fully validated by the `accepts_offer` match, so the four
                // single-service guards below are skipped for it. Empty
                // `accepted_offers` ⇒ `co_offered` is always false ⇒ exact
                // single-service behaviour.
                let co_offered = config.accepts_offer(
                    entry_view.service_id(),
                    entry_view.instance_id(),
                    entry_view.major_version(),
                    entry_view.event_group_id(),
                );

                if !co_offered && entry_view.service_id() != config.service_id {
                    crate::log::warn!(
                        "Subscribe for wrong service: expected 0x{:04X}, got 0x{:04X}",
                        config.service_id,
                        entry_view.service_id()
                    );
                    if let Err(e) = send_subscribe_nack_from_view(
                        send_buf,
                        config,
                        sd_socket,
                        sd_state,
                        &entry_view,
                        sender,
                        "wrong_service_id",
                    )
                    .await
                    {
                        crate::log::warn!("SubscribeNack send failed: {e}");
                    }
                } else if !co_offered && entry_view.instance_id() != config.instance_id {
                    crate::log::warn!(
                        "Subscribe for wrong instance: expected {}, got {}",
                        config.instance_id,
                        entry_view.instance_id()
                    );
                    if let Err(e) = send_subscribe_nack_from_view(
                        send_buf,
                        config,
                        sd_socket,
                        sd_state,
                        &entry_view,
                        sender,
                        "wrong_instance_id",
                    )
                    .await
                    {
                        crate::log::warn!("SubscribeNack send failed: {e}");
                    }
                } else if !co_offered && entry_view.major_version() != config.major_version {
                    crate::log::warn!(
                        "Subscribe for wrong major_version: expected {}, got {}",
                        config.major_version,
                        entry_view.major_version()
                    );
                    if let Err(e) = send_subscribe_nack_from_view(
                        send_buf,
                        config,
                        sd_socket,
                        sd_state,
                        &entry_view,
                        sender,
                        "wrong_major_version",
                    )
                    .await
                    {
                        crate::log::warn!("SubscribeNack send failed: {e}");
                    }
                } else if !co_offered && !config.accepts_event_group(entry_view.event_group_id()) {
                    crate::log::warn!(
                        "Subscribe for unknown event_group_id 0x{:04X} (service 0x{:04X})",
                        entry_view.event_group_id(),
                        entry_view.service_id()
                    );
                    if let Err(e) = send_subscribe_nack_from_view(
                        send_buf,
                        config,
                        sd_socket,
                        sd_state,
                        &entry_view,
                        sender,
                        "unknown_event_group",
                    )
                    .await
                    {
                        crate::log::warn!("SubscribeNack send failed: {e}");
                    }
                } else if let Some(endpoint_addr) = extract_subscriber_endpoint(entry_options) {
                    let subscribe_result = subscriptions
                        .subscribe(
                            entry_view.service_id(),
                            entry_view.instance_id(),
                            entry_view.event_group_id(),
                            endpoint_addr,
                        )
                        .await;

                    match subscribe_result {
                        Ok(()) => {
                            if let Err(e) = send_subscribe_ack_from_view(
                                send_buf,
                                config,
                                sd_socket,
                                sd_state,
                                &entry_view,
                                sender,
                            )
                            .await
                            {
                                crate::log::warn!(
                                    "SubscribeAck send failed; rolling back subscription \
                                         (service_id=0x{:04X}, instance_id={}, \
                                         event_group_id=0x{:04X}, error={e})",
                                    entry_view.service_id(),
                                    entry_view.instance_id(),
                                    entry_view.event_group_id(),
                                );
                                subscriptions
                                    .unsubscribe(
                                        entry_view.service_id(),
                                        entry_view.instance_id(),
                                        entry_view.event_group_id(),
                                        endpoint_addr,
                                    )
                                    .await;
                            }
                        }
                        Err(e) => {
                            let reason: &'static str = match e {
                                SubscribeError::SubscribersPerGroupFull => {
                                    "subscribers_per_group_full"
                                }
                                SubscribeError::EventGroupsFull => "event_groups_full",
                            };
                            crate::log::debug!("Subscription rejected: {reason}");
                            if let Err(e) = send_subscribe_nack_from_view(
                                send_buf,
                                config,
                                sd_socket,
                                sd_state,
                                &entry_view,
                                sender,
                                reason,
                            )
                            .await
                            {
                                crate::log::warn!("SubscribeNack send failed: {e}");
                            }
                        }
                    }
                } else {
                    crate::log::warn!("No endpoint found in Subscribe message options");
                    if let Err(e) = send_subscribe_nack_from_view(
                        send_buf,
                        config,
                        sd_socket,
                        sd_state,
                        &entry_view,
                        sender,
                        "no_endpoint_in_options",
                    )
                    .await
                    {
                        crate::log::warn!("SubscribeNack send failed: {e}");
                    }
                }
            }
            sd::EntryType::FindService => {
                let find_service_id = entry_view.service_id();
                if find_service_id == config.service_id || find_service_id == 0xFFFF {
                    crate::log::debug!(
                        "Received FindService from {} for service 0x{:04X} (ours: 0x{:04X}), sending unicast offer",
                        sender,
                        find_service_id,
                        config.service_id
                    );
                    if let Err(e) =
                        send_unicast_offer(send_buf, config, sd_socket, sd_state, sender).await
                    {
                        crate::log::warn!("Unicast OfferService send failed: {e}");
                    }
                } else {
                    crate::log::trace!(
                        "Ignoring FindService for service 0x{:04X} (not ours)",
                        find_service_id
                    );
                }
            }
            sd::EntryType::OfferService if !entry_view.is_stop_offer() => {
                for (request, answered) in config.consumed_eventgroups.iter().zip(&mut answered) {
                    *answered |= request.service_id == entry_view.service_id()
                        && request.instance_id == entry_view.instance_id()
                        && request.major_version == entry_view.major_version();
                }
            }
            _ => {
                crate::log::trace!("Ignoring SD entry type: {:?}", entry_type);
            }
        }
    }

    for (flag, _) in offered
        .iter()
        .zip(&answered)
        .filter(|(_, answered)| **answered)
    {
        flag.store(true, core::sync::atomic::Ordering::Relaxed);
    }

    if answered.contains(&true)
        && let Err(e) =
            send_subscribes(send_buf, config, sd_socket, sd_state, &answered, sender).await
    {
        crate::log::warn!("Subscribe send failed: {e}");
    }

    Ok(())
}

/// Periodic SD `OfferService` announcement loop. Runs forever; intended
/// to be combined with the receive loop via [`run_combined`].
pub(super) async fn announce_loop<T, Tm>(
    config: &ServerConfig,
    sd_socket: &T,
    sd_state: &SdStateManager,
    timer: &Tm,
    announce_send_buf: &mut [u8],
) where
    T: TransportSocket,
    Tm: Timer,
{
    let mut announcement_count = 0u32;
    loop {
        match sd_state
            .send_offer_service(announce_send_buf, config, sd_socket)
            .await
        {
            Ok(()) => {
                announcement_count += 1;
                if announcement_count == 1 {
                    crate::log::info!(
                        "Sent first SD announcement for service 0x{:04X}",
                        config.service_id
                    );
                } else {
                    crate::log::debug!(
                        "Sent {} SD announcements for service 0x{:04X}",
                        announcement_count,
                        config.service_id
                    );
                }
            }
            Err(e) => {
                crate::log::error!("Failed to send OfferService: {:?}", e);
            }
        }
        timer.sleep(core::time::Duration::from_secs(1)).await;
    }
}

/// Dispatch a non-SD unicast request (a method call to an offered service)
/// to the observer. If the observer produces a getter response (a
/// non-negative length), frame the SOME/IP RESPONSE — echoing the request's
/// id and protocol/interface versions — and send it back to `source`.
/// `send_buf` must be distinct from the buffer `view` borrows: the handler
/// writes its response payload after the header slot, so they don't alias.
async fn dispatch_non_sd_request<T: TransportSocket, R: E2EHandle + ?Sized>(
    unicast_socket: &T,
    observer: (super::NonSdRequestCallback, usize),
    e2e: &R,
    view: &crate::protocol::MessageView<'_>,
    source: core::net::SocketAddrV4,
    send_buf: &mut [u8],
) {
    let (cb, ctx) = observer;
    let hdr = view.header();
    let id = hdr.message_id();
    let (service_id, method_id) = (id.service_id(), id.method_id());
    // A request that passes the hook's check arrives without its
    // protection header; any other request arrives as received.
    let parsed = crate::sd_codec::ParsedDatagram {
        service_id,
        method_id,
        upper_header: hdr.upper_header_bytes(),
        payload: view.payload_bytes(),
    };
    let (status, body) =
        crate::sd_codec::check_parsed_e2e(e2e, core::net::IpAddr::V4(*source.ip()), &parsed);
    let resp_len = cb(
        ctx,
        source,
        service_id,
        method_id,
        body,
        status,
        &mut send_buf[crate::sd_codec::SOMEIP_HEADER_LEN..],
    );
    // A negative length means "no response" (a setter / fire-and-forget).
    let Ok(payload_len) = usize::try_from(resp_len) else {
        return;
    };
    // The callback was handed `&mut send_buf[SOMEIP_HEADER_LEN..]`, so a
    // response claiming more than that slice holds is a contract violation.
    // Drop it rather than slice `send_buf[..SOMEIP_HEADER_LEN + payload_len]`
    // out of bounds: the callback is consumer/FFI code, and on the bare-metal
    // runtime the panic would unwind across the `extern "C"` boundary (UB).
    let usable = send_buf.len() - crate::sd_codec::SOMEIP_HEADER_LEN;
    if payload_len > usable {
        crate::log::warn!(
            "non-SD response length {} exceeds {}-byte response buffer; dropped",
            payload_len,
            usable
        );
        return;
    }
    if crate::sd_codec::encode_response_header(
        send_buf,
        service_id,
        method_id,
        hdr.request_id(),
        hdr.protocol_version(),
        hdr.interface_version(),
        payload_len,
    )
    .is_ok()
    {
        let total = crate::sd_codec::SOMEIP_HEADER_LEN + payload_len;
        let total = match crate::transport::protect_in_place(e2e, send_buf, total) {
            Ok(total) => total,
            Err(e) => {
                crate::log::warn!("protecting the non-SD response failed: {:?}; dropped", e);
                return;
            }
        };
        if let Err(e) = unicast_socket.send_to(&send_buf[..total], source).await {
            crate::log::warn!("non-SD response send failed: {:?}", e);
        }
    }
}

/// Receive loop body — drives `recv_from` on both the unicast and SD
/// sockets, dispatches SD messages to [`handle_sd_message`] and non-SD
/// unicast requests to [`dispatch_non_sd_request`].
#[allow(clippy::too_many_arguments)]
async fn recv_loop<T, Sub, R>(
    config: &ServerConfig,
    unicast_socket: &T,
    sd_socket: &T,
    sd_state: &SdStateManager,
    subscriptions: &Sub,
    e2e: &R,
    unicast_buf: &mut [u8],
    sd_buf: &mut [u8],
    send_buf: &mut [u8],
    non_sd_observer: Option<(super::NonSdRequestCallback, usize)>,
    offered: &[core::sync::atomic::AtomicBool],
) -> Result<(), Error>
where
    T: TransportSocket,
    Sub: SubscriptionHandle,
    R: E2EHandle + Clone,
{
    use crate::protocol::MessageView;

    // Iteration counter used to flip `select_biased!` arm priority
    // each turn. We can't use the pseudo-random `select!` (it needs
    // `std`), so flipping arm order each iteration approximates the
    // fairness it would give without pulling std — a sustained
    // one-sided load (only-unicast or only-sd) cannot starve the
    // other arm.
    let mut prefer_sd_first = false;
    loop {
        // Both arms call `TransportSocket::recv_from`, whose contract
        // (see the trait docs) requires the returned future be
        // cancel-safe — dropping a non-selected arm must not lose
        // in-flight kernel state. The `TokioSocket` backend satisfies
        // this; custom backends must too. A future contributor adding
        // a non-cancel-safe arm here would silently lose datagrams
        // when the arm is dropped on a select win.
        //
        // Fresh futures are constructed each iteration so the borrows
        // of `unicast_buf` / `sd_buf` / the sockets end when the
        // select macro returns, freeing the buffer we index into
        // below. Each arm returns just `(datagram, from_unicast)`;
        // the `(len, addr, source)` derivation lives once below the
        // select so the arm-flip pattern doesn't duplicate it.
        let (datagram, from_unicast) = {
            let unicast_fut = unicast_socket.recv_from(&mut *unicast_buf).fuse();
            let sd_fut = sd_socket.recv_from(&mut *sd_buf).fuse();
            pin_mut!(unicast_fut, sd_fut);
            if prefer_sd_first {
                select_biased! {
                    result = sd_fut => (result?, false),
                    result = unicast_fut => (result?, true),
                }
            } else {
                select_biased! {
                    result = unicast_fut => (result?, true),
                    result = sd_fut => (result?, false),
                }
            }
        };
        prefer_sd_first = !prefer_sd_first;
        let len = datagram.bytes_received;
        let addr = core::net::SocketAddr::V4(datagram.source);
        let source = if from_unicast {
            "unicast"
        } else {
            "sd-multicast"
        };
        // The `datagram.truncated` flag is currently not surfaced via
        // `crate::log::warn!` — backends that report truncation honestly
        // (embassy-net today, tokio after #119) won't be observable
        // from the server side until #120 lands.
        let data = if from_unicast {
            &unicast_buf[..len]
        } else {
            &sd_buf[..len]
        };

        crate::log::trace!("Received {} bytes from {} on {} socket", len, addr, source);
        crate::log::trace!("Raw data: {:02X?}", &data[..len.min(64_usize)]);

        match MessageView::parse(data) {
            Ok(view) => {
                crate::log::trace!(
                    "SOME/IP Header: service=0x{:04X}, method=0x{:04X}, type={:?}",
                    view.header().message_id().service_id(),
                    view.header().message_id().method_id(),
                    view.header().message_type().message_type()
                );

                if view.is_sd() {
                    crate::log::trace!("This is an SD message");
                    match view.sd_header() {
                        Ok(sd_view) => {
                            crate::log::trace!("SD message has {} entries", sd_view.entry_count());
                            handle_sd_message(
                                config,
                                sd_socket,
                                sd_state,
                                subscriptions,
                                &sd_view,
                                addr,
                                send_buf,
                                offered,
                            )
                            .await?;
                        }
                        Err(e) => {
                            crate::log::warn!("Failed to parse SD message: {:?}", e);
                        }
                    }
                } else if from_unicast {
                    // Non-SD unicast = a method request to an offered service.
                    if let Some(observer) = non_sd_observer {
                        if let core::net::SocketAddr::V4(src_v4) = addr {
                            dispatch_non_sd_request(
                                unicast_socket,
                                observer,
                                e2e,
                                &view,
                                src_v4,
                                send_buf,
                            )
                            .await;
                        }
                    } else {
                        crate::log::trace!(
                            "Non-SD unicast SOME/IP message, no observer registered — ignoring"
                        );
                    }
                } else {
                    crate::log::trace!("Non-SD multicast SOME/IP message, ignoring");
                }
            }
            Err(e) => {
                crate::log::warn!("Failed to parse SOME/IP header from {}: {:?}", addr, e);
                crate::log::trace!("Data: {:02X?}", &data[..len.min(32)]);
            }
        }
    }
}

/// Encode one SD message with a `FindService` entry for each consumed
/// service instance not yet offered; `None` when every one is.
fn encode_finds(
    buf: &mut [u8],
    config: &ServerConfig,
    sd_state: &SdStateManager,
    offered: &[core::sync::atomic::AtomicBool],
) -> Result<Option<usize>, Error> {
    let consumed = &config.consumed_eventgroups;
    let same_instance = |a: &crate::sd_codec::SubscribeEventgroupRequest,
                         b: &crate::sd_codec::SubscribeEventgroupRequest| {
        (a.service_id, a.instance_id, a.major_version)
            == (b.service_id, b.instance_id, b.major_version)
    };
    let mut wanted = consumed.clone();
    let mut index = 0;
    wanted.retain(|request| {
        let i = index;
        index += 1;
        let is_offered = offered
            .get(i)
            .is_some_and(|f| f.load(core::sync::atomic::Ordering::Relaxed));
        !is_offered
            && !consumed[..i]
                .iter()
                .any(|earlier| same_instance(earlier, request))
    });
    if wanted.is_empty() {
        return Ok(None);
    }
    let (sid, reboot_flag) = sd_state.next_session_id_with_reboot_flag();
    #[allow(clippy::cast_possible_truncation)]
    let session = sid as u16;
    crate::sd_codec::build_find_service_datagram::<{ ServerConfig::CONSUMED_EVENTGROUPS_CAP }>(
        buf,
        &wanted,
        session,
        reboot_flag,
    )
    .map(Some)
    .map_err(|_| Error::Capacity(CapacityKind::UdpBuffer))
}

const FIND_BUF_LEN: usize = 16 + 12 + 16 * ServerConfig::CONSUMED_EVENTGROUPS_CAP;

/// Send `FindService` for the consumed service instances not yet offered on
/// [`ServerConfig::find_timing`]'s schedule; returns after the last send.
pub(super) async fn find_loop<T, Tm>(
    config: &ServerConfig,
    sd_socket: &T,
    sd_state: &SdStateManager,
    timer: &Tm,
    offered: &[core::sync::atomic::AtomicBool],
) where
    T: TransportSocket,
    Tm: Timer,
{
    let timing = config.find_timing;
    let mut buf = [0u8; FIND_BUF_LEN];
    let mut delay = timing.initial_delay;
    for repetition in 0..=u32::from(timing.repetitions_max) {
        timer.sleep(delay).await;
        match encode_finds(&mut buf, config, sd_state, offered) {
            Ok(Some(len)) => {
                if let Err(e) = sd_socket.send_to(&buf[..len], timing.sd_multicast).await {
                    crate::log::warn!("FindService send failed: {e:?}");
                }
            }
            Ok(None) => return,
            Err(e) => crate::log::warn!("FindService encode failed: {e:?}"),
        }
        delay = timing
            .repetitions_base_delay
            .saturating_mul(2u32.saturating_pow(repetition));
    }
}

/// Combined receive + announce loop. The single future returned from
/// `Server::new` (and friends) drives this; it is also what
/// [`Server::run_with_buffers`] resolves to once buffers are
/// supplied.
///
/// Returns `Err(Error::InvalidUsage("passive_server_run"))` if invoked
/// on a passive server (passive servers have no SD socket bound to
/// 30490 and rely on an external SD dispatcher).
///
/// When `config.announce` is `false`, the announcement arm is skipped
/// and only the receive loop drives — used by the dispatcher topology
/// where a co-located `Client` emits `OfferService` on the server's
/// behalf.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_combined<H, T, Sub, Hsd, Tm, R>(
    config: ServerConfig,
    unicast_socket: H,
    sd_socket: H,
    subscriptions: Sub,
    sd_state: Hsd,
    e2e: R,
    timer: Tm,
    is_passive: bool,
    unicast_buf: &mut [u8],
    sd_buf: &mut [u8],
    recv_send_buf: &mut [u8],
    announce_send_buf: &mut [u8],
    non_sd_observer: Option<(super::NonSdRequestCallback, usize)>,
) -> Result<(), Error>
where
    H: SharedHandle<T>,
    T: TransportSocket + 'static,
    Sub: SubscriptionHandle,
    Hsd: SharedHandle<SdStateManager>,
    Tm: Timer,
    R: E2EHandle + Clone,
{
    if is_passive {
        crate::log::warn!(
            "run called on passive Server for service 0x{:04X}; \
             SD receive must be driven externally (e.g. via the \
             Client's discovery socket, routing Subscribes to \
             `EventPublisher::register_subscriber`)",
            config.service_id
        );
        return Err(Error::InvalidUsage("passive_server_run"));
    }

    let unicast = unicast_socket.get();
    let sd = sd_socket.get();
    let sd_state_ref = sd_state.get();

    let offered = [const { core::sync::atomic::AtomicBool::new(false) };
        ServerConfig::CONSUMED_EVENTGROUPS_CAP];
    let recv_fut = recv_loop(
        &config,
        unicast,
        sd,
        sd_state_ref,
        &subscriptions,
        &e2e,
        unicast_buf,
        sd_buf,
        recv_send_buf,
        non_sd_observer,
        &offered,
    );
    let announce_fut = async {
        if config.announce {
            announce_loop(&config, sd, sd_state_ref, &timer, announce_send_buf).await;
        }
        core::future::pending::<()>().await;
    };
    let find_fut = async {
        if !config.consumed_eventgroups.is_empty() {
            find_loop(&config, sd, sd_state_ref, &timer, &offered).await;
        }
        core::future::pending::<()>().await;
    };
    let others = futures_util::future::join(announce_fut, find_fut);
    pin_mut!(recv_fut, others);
    match futures_util::future::select(recv_fut, others).await {
        Either::Left((recv_result, _)) => recv_result,
        Either::Right((_, recv_pending)) => recv_pending.await,
    }
}

fn socket_addr_v4(addr: core::net::SocketAddr) -> Result<SocketAddrV4, Error> {
    match addr {
        core::net::SocketAddr::V4(v4) => Ok(v4),
        core::net::SocketAddr::V6(_) => Err(Error::Transport(
            crate::transport::TransportError::Unsupported,
        )),
    }
}

/// The first IPv4 endpoint among `options`, the options a Subscribe entry
/// references.
pub(super) fn extract_subscriber_endpoint<'a>(
    options: impl Iterator<Item = sd::OptionView<'a>>,
) -> Option<SocketAddrV4> {
    let mut first_endpoint: Option<SocketAddrV4> = None;
    let mut endpoint_count: usize = 0;
    let mut ignored_other: usize = 0;

    for option_view in options {
        match option_view.option_type() {
            Ok(sd::OptionType::IpV4Endpoint) => {
                if let Ok((ip, _, port)) = option_view.as_ipv4() {
                    endpoint_count += 1;
                    if first_endpoint.is_none() {
                        first_endpoint = Some(SocketAddrV4::new(ip, port));
                    }
                }
            }
            Ok(_) | Err(_) => ignored_other += 1,
        }
    }

    match endpoint_count {
        0 => {
            crate::log::warn!(
                "No IPv4 endpoint among the options the Subscribe references \
                 (ignored={ignored_other})"
            );
            None
        }
        1 => {
            let ep = first_endpoint.expect("endpoint_count=1 implies first_endpoint is Some");
            crate::log::trace!("Found IPv4 endpoint {}", ep);
            Some(ep)
        }
        n => {
            let ep = first_endpoint.expect("endpoint_count>=1 implies first_endpoint is Some");
            crate::log::warn!(
                "{} IPv4 endpoints found in subscribe options runs; \
                 using first ({}) and ignoring {} additional. \
                 Multi-endpoint (e.g. TCP+UDP) subscribers are not yet supported.",
                n,
                ep,
                n - 1
            );
            Some(ep)
        }
    }
}

// ── Unit tests for SD send helpers ───────────────────────────────────────────
//
// These tests live in `runtime.rs` (rather than `tests/bare_metal_e2e.rs`)
// because the three helpers are `pub(super)` and are not reachable from
// integration-test crates.  Task 3 will expose a public surface (via
// `run_with_buffers` threading the real scratch) that integration tests can
// exercise end-to-end; until then, the helper-level contract is verified here.
#[cfg(test)]
mod tests {
    use super::*;

    use core::net::{Ipv4Addr, SocketAddrV4};

    use crate::transport::{ReceivedDatagram, TransportError, TransportSocket};

    // ── Minimal no-op mock socket ─────────────────────────────────────────

    struct NullSocket;

    struct NullSend;
    impl core::future::Future for NullSend {
        type Output = Result<(), TransportError>;
        fn poll(
            self: core::pin::Pin<&mut Self>,
            _cx: &mut core::task::Context<'_>,
        ) -> core::task::Poll<Self::Output> {
            core::task::Poll::Ready(Ok(()))
        }
    }

    struct NullRecv;
    impl core::future::Future for NullRecv {
        type Output = Result<ReceivedDatagram, TransportError>;
        fn poll(
            self: core::pin::Pin<&mut Self>,
            _cx: &mut core::task::Context<'_>,
        ) -> core::task::Poll<Self::Output> {
            core::task::Poll::Pending
        }
    }

    impl TransportSocket for NullSocket {
        type SendFuture<'a> = NullSend;
        type RecvFuture<'a> = NullRecv;

        fn send_to<'a>(&'a self, _buf: &'a [u8], _target: SocketAddrV4) -> Self::SendFuture<'a> {
            NullSend
        }
        fn recv_from<'a>(&'a self, _buf: &'a mut [u8]) -> Self::RecvFuture<'a> {
            NullRecv
        }
        fn local_addr(&self) -> Result<SocketAddrV4, TransportError> {
            Ok(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
        }
        fn join_multicast_v4(
            &self,
            _group: Ipv4Addr,
            _iface: Ipv4Addr,
        ) -> Result<(), TransportError> {
            Ok(())
        }
        fn leave_multicast_v4(
            &self,
            _group: Ipv4Addr,
            _iface: Ipv4Addr,
        ) -> Result<(), TransportError> {
            Ok(())
        }
    }

    fn make_config() -> ServerConfig {
        ServerConfig::new(0x1234, 1)
            .with_interface(Ipv4Addr::LOCALHOST)
            .with_local_port(30500)
    }

    fn make_sd_state() -> SdStateManager {
        SdStateManager::new()
    }

    fn subscriber_addr() -> core::net::SocketAddr {
        core::net::SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 40000))
    }

    // ── Task 1 RED/GREEN: undersized buf rejects with Capacity, not panic ─

    /// `send_unicast_offer` with a 24-byte buf (fits 16-byte header but
    /// not the SD payload) must return `Err(Capacity(CapacityKind::UdpBuffer))`.
    #[tokio::test]
    async fn send_unicast_offer_undersized_buf_returns_capacity() {
        let config = make_config();
        let sd_state = make_sd_state();
        let socket = NullSocket;
        let target = subscriber_addr();

        let result = send_unicast_offer(&mut [0u8; 24], &config, &socket, &sd_state, target).await;

        assert!(
            matches!(result, Err(Error::Capacity(CapacityKind::UdpBuffer))),
            "expected Capacity(\"udp_buffer\"), got {result:?}"
        );
    }

    /// `send_unicast_offer` with a buf shorter than the 16-byte SOME/IP
    /// header must return `Err(Capacity(CapacityKind::UdpBuffer))`.
    #[tokio::test]
    async fn send_unicast_offer_buf_shorter_than_header_returns_capacity() {
        let config = make_config();
        let sd_state = make_sd_state();
        let socket = NullSocket;
        let target = subscriber_addr();

        let result = send_unicast_offer(&mut [0u8; 8], &config, &socket, &sd_state, target).await;

        assert!(
            matches!(result, Err(Error::Capacity(CapacityKind::UdpBuffer))),
            "expected Capacity(\"udp_buffer\"), got {result:?}"
        );
    }

    /// `send_unicast_offer` with a full-size buf must succeed (no regression).
    #[tokio::test]
    async fn send_unicast_offer_full_size_buf_succeeds() {
        let config = make_config();
        let sd_state = make_sd_state();
        let socket = NullSocket;
        let target = subscriber_addr();

        let result = send_unicast_offer(
            &mut [0u8; crate::UDP_BUFFER_SIZE],
            &config,
            &socket,
            &sd_state,
            target,
        )
        .await;

        assert!(result.is_ok(), "full-size buf must succeed, got {result:?}");
    }

    // ── send_subscribe_ack_from_view: build a minimal EntryView via wire bytes

    /// Encode a minimal Subscribe SD payload and return `(wire_bytes, sd_len)` so
    /// callers can parse an `SdHeaderView` and extract an `EntryView`.
    fn subscribe_wire_bytes() -> ([u8; 512], usize) {
        use automotive_wire_codec::Encode;

        let entry = sd::Entry::SubscribeEventGroup(sd::EventGroupEntry {
            index_first_options_run: 0,
            index_second_options_run: 0,
            options_count: sd::OptionsCount::new(1, 0),
            service_id: 0x1234,
            instance_id: 1,
            major_version: 1,
            ttl: 3,
            counter: 0,
            event_group_id: 0x0001,
        });
        let option = sd::Options::IpV4Endpoint {
            ip: Ipv4Addr::LOCALHOST,
            port: 40000,
            protocol: sd::TransportProtocol::Udp,
        };
        let entries = [entry];
        let options = [option];
        let sd_payload = sd::Header::new(
            sd::Flags::new_sd(sd::RebootFlag::RecentlyRebooted),
            &entries,
            &options,
        );

        let mut wire = [0u8; 512];
        let sd_len = sd_payload.encode_to_slice(&mut wire).expect("encode");
        (wire, sd_len)
    }

    /// `send_subscribe_ack_from_view` with a 24-byte buf must return
    /// `Err(Capacity(CapacityKind::UdpBuffer))` without panicking.
    #[tokio::test]
    async fn send_subscribe_ack_undersized_buf_returns_capacity_not_panic() {
        let config = make_config();
        let sd_state = make_sd_state();
        let socket = NullSocket;
        let subscriber = subscriber_addr();

        let (wire, sd_len) = subscribe_wire_bytes();
        let sd_view = sd::SdHeaderView::parse(&wire[..sd_len]).expect("parse");
        let entry_view = sd_view.entries().next().expect("one entry");

        let result = send_subscribe_ack_from_view(
            &mut [0u8; 24],
            &config,
            &socket,
            &sd_state,
            &entry_view,
            subscriber,
        )
        .await;

        assert!(
            matches!(result, Err(Error::Capacity(CapacityKind::UdpBuffer))),
            "expected Capacity(\"udp_buffer\"), got {result:?}"
        );
    }

    /// `send_subscribe_ack_from_view` with a full-size buf must succeed.
    #[tokio::test]
    async fn send_subscribe_ack_full_size_buf_succeeds() {
        let config = make_config();
        let sd_state = make_sd_state();
        let socket = NullSocket;
        let subscriber = subscriber_addr();

        let (wire, sd_len) = subscribe_wire_bytes();
        let sd_view = sd::SdHeaderView::parse(&wire[..sd_len]).expect("parse");
        let entry_view = sd_view.entries().next().expect("one entry");

        let result = send_subscribe_ack_from_view(
            &mut [0u8; crate::UDP_BUFFER_SIZE],
            &config,
            &socket,
            &sd_state,
            &entry_view,
            subscriber,
        )
        .await;

        assert!(result.is_ok(), "full-size buf must succeed, got {result:?}");
    }

    /// `send_subscribe_nack_from_view` with a 24-byte buf must return
    /// `Err(Capacity(CapacityKind::UdpBuffer))` without panicking.
    #[tokio::test]
    async fn send_subscribe_nack_undersized_buf_returns_capacity_not_panic() {
        let config = make_config();
        let sd_state = make_sd_state();
        let socket = NullSocket;
        let subscriber = subscriber_addr();

        let (wire, sd_len) = subscribe_wire_bytes();
        let sd_view = sd::SdHeaderView::parse(&wire[..sd_len]).expect("parse");
        let entry_view = sd_view.entries().next().expect("one entry");

        let result = send_subscribe_nack_from_view(
            &mut [0u8; 24],
            &config,
            &socket,
            &sd_state,
            &entry_view,
            subscriber,
            "test_reason",
        )
        .await;

        assert!(
            matches!(result, Err(Error::Capacity(CapacityKind::UdpBuffer))),
            "expected Capacity(\"udp_buffer\"), got {result:?}"
        );
    }

    /// `send_subscribe_nack_from_view` with a full-size buf must succeed.
    #[tokio::test]
    async fn send_subscribe_nack_full_size_buf_succeeds() {
        let config = make_config();
        let sd_state = make_sd_state();
        let socket = NullSocket;
        let subscriber = subscriber_addr();

        let (wire, sd_len) = subscribe_wire_bytes();
        let sd_view = sd::SdHeaderView::parse(&wire[..sd_len]).expect("parse");
        let entry_view = sd_view.entries().next().expect("one entry");

        let result = send_subscribe_nack_from_view(
            &mut [0u8; crate::UDP_BUFFER_SIZE],
            &config,
            &socket,
            &sd_state,
            &entry_view,
            subscriber,
            "test_reason",
        )
        .await;

        assert!(result.is_ok(), "full-size buf must succeed, got {result:?}");
    }

    #[cfg(feature = "server-tokio")]
    mod consumed_eventgroups {
        use super::*;
        use crate::server::FindTiming;

        /// Records every datagram sent through it.
        #[derive(Default)]
        struct RecordingSocket {
            sent: std::sync::Mutex<std::vec::Vec<(std::vec::Vec<u8>, SocketAddrV4)>>,
        }

        impl TransportSocket for RecordingSocket {
            type SendFuture<'a> = NullSend;
            type RecvFuture<'a> = NullRecv;

            fn send_to<'a>(&'a self, buf: &'a [u8], target: SocketAddrV4) -> Self::SendFuture<'a> {
                self.sent.lock().unwrap().push((buf.to_vec(), target));
                NullSend
            }
            fn recv_from<'a>(&'a self, _buf: &'a mut [u8]) -> Self::RecvFuture<'a> {
                NullRecv
            }
            fn local_addr(&self) -> Result<SocketAddrV4, TransportError> {
                Ok(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            }
            fn join_multicast_v4(
                &self,
                _group: Ipv4Addr,
                _iface: Ipv4Addr,
            ) -> Result<(), TransportError> {
                Ok(())
            }
            fn leave_multicast_v4(
                &self,
                _group: Ipv4Addr,
                _iface: Ipv4Addr,
            ) -> Result<(), TransportError> {
                Ok(())
            }
        }

        const PEER_SD: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 2), 30490);

        fn consumed() -> crate::sd_codec::SubscribeEventgroupRequest {
            crate::sd_codec::SubscribeEventgroupRequest {
                service_id: 0x5678,
                instance_id: 1,
                major_version: 1,
                event_group_id: 0x0001,
                ttl: 5,
                local_ip: Ipv4Addr::LOCALHOST,
                local_rx_port: 40100,
            }
        }

        /// An SD payload with one Offer entry for `service_id` with `ttl`; a TTL of
        /// 0 makes it a `StopOffer`.
        fn offer_wire_bytes(service_id: u16, ttl: u32) -> ([u8; 512], usize) {
            use automotive_wire_codec::Encode;

            let service = sd::ServiceEntry {
                index_first_options_run: 0,
                index_second_options_run: 0,
                options_count: sd::OptionsCount::new(1, 0),
                service_id,
                instance_id: 1,
                major_version: 1,
                ttl,
                minor_version: 0,
            };
            let entry = if ttl == 0 {
                sd::Entry::StopOfferService(service)
            } else {
                sd::Entry::OfferService(service)
            };
            let option = sd::Options::IpV4Endpoint {
                ip: *PEER_SD.ip(),
                port: 30509,
                protocol: sd::TransportProtocol::Udp,
            };
            let entries = [entry];
            let options = [option];
            let sd_payload = sd::Header::new(
                sd::Flags::new_sd(sd::RebootFlag::RecentlyRebooted),
                &entries,
                &options,
            );
            let mut wire = [0u8; 512];
            let sd_len = sd_payload.encode_to_slice(&mut wire).expect("encode");
            (wire, sd_len)
        }

        async fn handle_offer(config: &ServerConfig, service_id: u16, ttl: u32) -> RecordingSocket {
            let socket = RecordingSocket::default();
            let (wire, sd_len) = offer_wire_bytes(service_id, ttl);
            let sd_view = sd::SdHeaderView::parse(&wire[..sd_len]).expect("parse");
            let subscriptions = crate::server::SubscriptionManager::new();
            let subscriptions = std::sync::Arc::new(tokio::sync::RwLock::new(subscriptions));
            handle_sd_message(
                config,
                &socket,
                &make_sd_state(),
                &subscriptions,
                &sd_view,
                core::net::SocketAddr::V4(PEER_SD),
                &mut [0u8; crate::UDP_BUFFER_SIZE],
                &offered_flags(),
            )
            .await
            .expect("handle_sd_message");
            socket
        }

        #[tokio::test]
        async fn offer_of_a_consumed_eventgroup_is_answered_with_a_unicast_subscribe() {
            let config = make_config().with_consumed_eventgroup(consumed());

            let socket = handle_offer(&config, 0x5678, 3).await;

            let sent = socket.sent.lock().unwrap();
            assert_eq!(sent.len(), 1, "one SD message answers the Offer");
            let (datagram, to) = &sent[0];
            assert_eq!(*to, PEER_SD, "the Subscribe goes to the Offer's sender");
            let view = crate::protocol::MessageView::parse(datagram).expect("SOME/IP");
            let sd_view = view.sd_header().expect("SD");
            let entries: std::vec::Vec<_> = sd_view.entries_with_options().collect();
            assert_eq!(entries.len(), 1);
            let (e, options) = entries.into_iter().next().unwrap();
            assert!(matches!(e.entry_type(), Ok(sd::EntryType::Subscribe)));
            assert_eq!(
                (e.service_id(), e.instance_id(), e.major_version()),
                (0x5678, 1, 1)
            );
            assert_eq!((e.event_group_id(), e.ttl()), (0x0001, 5));
            let endpoint = extract_subscriber_endpoint(options);
            assert_eq!(
                endpoint,
                Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 40100))
            );
        }

        #[tokio::test]
        async fn offer_of_an_unconsumed_service_is_not_answered() {
            let config = make_config().with_consumed_eventgroup(consumed());

            let socket = handle_offer(&config, 0x9999, 3).await;

            assert!(socket.sent.lock().unwrap().is_empty());
        }

        #[tokio::test]
        async fn stop_offer_of_a_consumed_eventgroup_is_not_answered() {
            let config = make_config().with_consumed_eventgroup(consumed());

            let socket = handle_offer(&config, 0x5678, 0).await;

            assert!(socket.sent.lock().unwrap().is_empty());
        }

        /// Fails every send.
        struct FailingSocket;

        struct FailSend;
        impl core::future::Future for FailSend {
            type Output = Result<(), TransportError>;
            fn poll(
                self: core::pin::Pin<&mut Self>,
                _cx: &mut core::task::Context<'_>,
            ) -> core::task::Poll<Self::Output> {
                core::task::Poll::Ready(Err(TransportError::Io(
                    crate::transport::IoErrorKind::Other,
                )))
            }
        }

        impl TransportSocket for FailingSocket {
            type SendFuture<'a> = FailSend;
            type RecvFuture<'a> = NullRecv;

            fn send_to<'a>(
                &'a self,
                _buf: &'a [u8],
                _target: SocketAddrV4,
            ) -> Self::SendFuture<'a> {
                FailSend
            }
            fn recv_from<'a>(&'a self, _buf: &'a mut [u8]) -> Self::RecvFuture<'a> {
                NullRecv
            }
            fn local_addr(&self) -> Result<SocketAddrV4, TransportError> {
                Ok(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            }
            fn join_multicast_v4(
                &self,
                _group: Ipv4Addr,
                _iface: Ipv4Addr,
            ) -> Result<(), TransportError> {
                Ok(())
            }
            fn leave_multicast_v4(
                &self,
                _group: Ipv4Addr,
                _iface: Ipv4Addr,
            ) -> Result<(), TransportError> {
                Ok(())
            }
        }

        /// An SD payload with an Offer entry for each of `offers`
        /// (`service_id`, `instance_id`, `major_version`).
        fn offers_wire_bytes(offers: &[(u16, u16, u8)]) -> ([u8; 512], usize) {
            use automotive_wire_codec::Encode;

            let entries: std::vec::Vec<_> = offers
                .iter()
                .map(|&(service_id, instance_id, major_version)| {
                    sd::Entry::OfferService(sd::ServiceEntry {
                        index_first_options_run: 0,
                        index_second_options_run: 0,
                        options_count: sd::OptionsCount::new(1, 0),
                        service_id,
                        instance_id,
                        major_version,
                        ttl: 3,
                        minor_version: 0,
                    })
                })
                .collect();
            let options = [sd::Options::IpV4Endpoint {
                ip: *PEER_SD.ip(),
                port: 30509,
                protocol: sd::TransportProtocol::Udp,
            }];
            let sd_payload = sd::Header::new(
                sd::Flags::new_sd(sd::RebootFlag::RecentlyRebooted),
                &entries,
                &options,
            );
            let mut wire = [0u8; 512];
            let sd_len = sd_payload.encode_to_slice(&mut wire).expect("encode");
            (wire, sd_len)
        }

        /// The (eventgroup, endpoint port) of each Subscribe in the one SD
        /// message `config` sends in answer to `offers`, or `None` if it
        /// sends nothing.
        async fn answer(
            config: &ServerConfig,
            offers: &[(u16, u16, u8)],
        ) -> Option<std::vec::Vec<(u16, u16)>> {
            let socket = RecordingSocket::default();
            let (wire, sd_len) = offers_wire_bytes(offers);
            let sd_view = sd::SdHeaderView::parse(&wire[..sd_len]).expect("parse");
            let subscriptions = std::sync::Arc::new(tokio::sync::RwLock::new(
                crate::server::SubscriptionManager::new(),
            ));
            handle_sd_message(
                config,
                &socket,
                &make_sd_state(),
                &subscriptions,
                &sd_view,
                core::net::SocketAddr::V4(PEER_SD),
                &mut [0u8; crate::UDP_BUFFER_SIZE],
                &offered_flags(),
            )
            .await
            .expect("handle_sd_message");
            let sent = socket.sent.lock().unwrap();
            assert!(
                sent.len() <= 1,
                "at most one SD message answers one message"
            );
            let (datagram, _) = sent.first()?;
            let view = crate::protocol::MessageView::parse(datagram).expect("SOME/IP");
            let sd_view = view.sd_header().expect("SD");
            Some(
                sd_view
                    .entries_with_options()
                    .map(|(e, options)| {
                        let endpoint = extract_subscriber_endpoint(options).expect("an endpoint");
                        (e.event_group_id(), endpoint.port())
                    })
                    .collect(),
            )
        }

        #[tokio::test]
        async fn offer_of_another_instance_or_major_version_is_not_answered() {
            let config = make_config().with_consumed_eventgroup(consumed());

            assert_eq!(answer(&config, &[(0x5678, 2, 1)]).await, None);
            assert_eq!(answer(&config, &[(0x5678, 1, 2)]).await, None);
        }

        #[tokio::test]
        async fn every_eventgroup_of_an_offered_service_is_subscribed_in_one_message() {
            let second = crate::sd_codec::SubscribeEventgroupRequest {
                event_group_id: 0x0002,
                local_rx_port: 40101,
                ..consumed()
            };
            let config = make_config()
                .with_consumed_eventgroup(consumed())
                .with_consumed_eventgroup(second);

            assert_eq!(
                answer(&config, &[(0x5678, 1, 1)]).await,
                Some(std::vec![(0x0001, 40100), (0x0002, 40101)])
            );
        }

        #[tokio::test]
        async fn only_the_offered_services_are_subscribed() {
            let other = crate::sd_codec::SubscribeEventgroupRequest {
                service_id: 0x5679,
                event_group_id: 0x0003,
                local_rx_port: 40102,
                ..consumed()
            };
            let config = make_config()
                .with_consumed_eventgroup(consumed())
                .with_consumed_eventgroup(other);

            assert_eq!(
                answer(&config, &[(0x9999, 1, 1), (0x5679, 1, 1)]).await,
                Some(std::vec![(0x0003, 40102)])
            );
        }

        #[tokio::test]
        async fn a_failed_subscribe_nack_send_does_not_end_sd_handling() {
            let config = ServerConfig::new(0x9999, 1);
            let (wire, sd_len) = subscribe_wire_bytes();
            let sd_view = sd::SdHeaderView::parse(&wire[..sd_len]).expect("parse");
            let subscriptions = std::sync::Arc::new(tokio::sync::RwLock::new(
                crate::server::SubscriptionManager::new(),
            ));

            let result = handle_sd_message(
                &config,
                &FailingSocket,
                &make_sd_state(),
                &subscriptions,
                &sd_view,
                subscriber_addr(),
                &mut [0u8; crate::UDP_BUFFER_SIZE],
                &offered_flags(),
            )
            .await;

            assert!(result.is_ok(), "{result:?}");
        }

        /// Returns at once from every `sleep`, recording its duration; on
        /// the sleep numbered `offer_at` (0-based) it marks `offered[0]`.
        struct RecordingTimer<'a> {
            sleeps: std::sync::Mutex<std::vec::Vec<core::time::Duration>>,
            offer_at: Option<(usize, &'a [core::sync::atomic::AtomicBool])>,
        }

        impl<'a> RecordingTimer<'a> {
            fn new() -> Self {
                Self {
                    sleeps: std::sync::Mutex::default(),
                    offer_at: None,
                }
            }
            fn offering_at(n: usize, offered: &'a [core::sync::atomic::AtomicBool]) -> Self {
                Self {
                    sleeps: std::sync::Mutex::default(),
                    offer_at: Some((n, offered)),
                }
            }
            fn millis(&self) -> std::vec::Vec<u128> {
                self.sleeps
                    .lock()
                    .unwrap()
                    .iter()
                    .map(core::time::Duration::as_millis)
                    .collect()
            }
        }

        impl crate::Timer for RecordingTimer<'_> {
            type SleepFuture<'a>
                = core::future::Ready<()>
            where
                Self: 'a;
            fn sleep(&self, duration: core::time::Duration) -> Self::SleepFuture<'_> {
                let mut sleeps = self.sleeps.lock().unwrap();
                if let Some((n, offered)) = self.offer_at
                    && sleeps.len() == n
                {
                    offered[0].store(true, core::sync::atomic::Ordering::Relaxed);
                }
                sleeps.push(duration);
                core::future::ready(())
            }
        }

        fn offered_flags()
        -> [core::sync::atomic::AtomicBool; ServerConfig::CONSUMED_EVENTGROUPS_CAP] {
            [const { core::sync::atomic::AtomicBool::new(false) };
                ServerConfig::CONSUMED_EVENTGROUPS_CAP]
        }

        /// The (service, instance) of each Find entry in each datagram sent.
        fn finds(socket: &RecordingSocket) -> std::vec::Vec<std::vec::Vec<(u16, u16)>> {
            socket
                .sent
                .lock()
                .unwrap()
                .iter()
                .map(|(d, to)| {
                    assert_eq!(*to, FindTiming::DEFAULT.sd_multicast);
                    let view = crate::protocol::MessageView::parse(d).expect("SOME/IP");
                    view.sd_header()
                        .expect("SD")
                        .entries()
                        .map(|e| {
                            assert!(matches!(e.entry_type(), Ok(sd::EntryType::FindService)));
                            (e.service_id(), e.instance_id())
                        })
                        .collect()
                })
                .collect()
        }

        fn once() -> FindTiming {
            FindTiming {
                repetitions_max: 0,
                ..FindTiming::DEFAULT
            }
        }

        #[tokio::test]
        async fn finds_after_the_initial_delay_then_doubling_repetitions() {
            let config = make_config().with_consumed_eventgroup(consumed());
            let (socket, timer, offered) = (
                RecordingSocket::default(),
                RecordingTimer::new(),
                offered_flags(),
            );

            find_loop(&config, &socket, &make_sd_state(), &timer, &offered).await;

            assert_eq!(timer.millis(), [10, 100, 200, 400]);
            assert_eq!(finds(&socket), std::vec![std::vec![(0x5678, 1)]; 4]);
        }

        #[tokio::test]
        async fn zero_repetitions_send_one_find() {
            let config = make_config()
                .with_consumed_eventgroup(consumed())
                .with_find_timing(once());
            let (socket, timer, offered) = (
                RecordingSocket::default(),
                RecordingTimer::new(),
                offered_flags(),
            );

            find_loop(&config, &socket, &make_sd_state(), &timer, &offered).await;

            assert_eq!(timer.millis(), [10]);
            assert_eq!(finds(&socket).len(), 1);
        }

        #[tokio::test]
        async fn an_instance_offered_before_the_first_send_is_never_found() {
            let config = make_config().with_consumed_eventgroup(consumed());
            let offered = offered_flags();
            let (socket, timer) = (
                RecordingSocket::default(),
                RecordingTimer::offering_at(0, &offered),
            );

            find_loop(&config, &socket, &make_sd_state(), &timer, &offered).await;

            assert!(finds(&socket).is_empty());
        }

        #[tokio::test]
        async fn an_offer_between_repetitions_stops_the_remaining_finds() {
            let config = make_config().with_consumed_eventgroup(consumed());
            let offered = offered_flags();
            let (socket, timer) = (
                RecordingSocket::default(),
                RecordingTimer::offering_at(2, &offered),
            );

            find_loop(&config, &socket, &make_sd_state(), &timer, &offered).await;

            assert_eq!(finds(&socket).len(), 2);
        }

        #[tokio::test]
        async fn eventgroups_of_one_instance_share_one_find_entry() {
            let second = crate::sd_codec::SubscribeEventgroupRequest {
                event_group_id: 0x0002,
                local_rx_port: 40101,
                ..consumed()
            };
            let other = crate::sd_codec::SubscribeEventgroupRequest {
                service_id: 0x5679,
                ..consumed()
            };
            let config = make_config()
                .with_consumed_eventgroup(consumed())
                .with_consumed_eventgroup(second)
                .with_consumed_eventgroup(other)
                .with_find_timing(once());
            let (socket, timer, offered) = (
                RecordingSocket::default(),
                RecordingTimer::new(),
                offered_flags(),
            );

            find_loop(&config, &socket, &make_sd_state(), &timer, &offered).await;

            assert_eq!(finds(&socket), [std::vec![(0x5678, 1), (0x5679, 1)]]);
        }

        #[tokio::test]
        async fn find_entries_carry_the_consumed_ttl_and_major_version() {
            let config = make_config()
                .with_consumed_eventgroup(consumed())
                .with_find_timing(once());
            let (socket, timer, offered) = (
                RecordingSocket::default(),
                RecordingTimer::new(),
                offered_flags(),
            );

            find_loop(&config, &socket, &make_sd_state(), &timer, &offered).await;

            let sent = socket.sent.lock().unwrap();
            let view = crate::protocol::MessageView::parse(&sent[0].0).expect("SOME/IP");
            let sd_view = view.sd_header().expect("SD");
            let e = sd_view.entries().next().expect("one entry");
            assert_eq!((e.major_version(), e.ttl()), (1, 5));
            assert_eq!(sd_view.options().count(), 0);
        }

        #[tokio::test]
        async fn a_failed_find_send_keeps_the_schedule() {
            let config = make_config().with_consumed_eventgroup(consumed());
            let (timer, offered) = (RecordingTimer::new(), offered_flags());

            find_loop(&config, &FailingSocket, &make_sd_state(), &timer, &offered).await;

            assert_eq!(timer.millis(), [10, 100, 200, 400]);
        }

        #[tokio::test]
        async fn a_huge_repetition_delay_saturates() {
            let timing = FindTiming {
                repetitions_base_delay: core::time::Duration::MAX,
                repetitions_max: 255,
                ..FindTiming::DEFAULT
            };
            let config = make_config()
                .with_consumed_eventgroup(consumed())
                .with_find_timing(timing);
            let (socket, timer, offered) = (
                RecordingSocket::default(),
                RecordingTimer::new(),
                offered_flags(),
            );

            find_loop(&config, &socket, &make_sd_state(), &timer, &offered).await;

            let sleeps = timer.sleeps.lock().unwrap();
            assert_eq!(sleeps.len(), 256);
            assert!(sleeps[1..].iter().all(|d| *d == core::time::Duration::MAX));
        }

        #[tokio::test]
        async fn an_answered_offer_marks_its_instance_offered() {
            let config = make_config().with_consumed_eventgroup(consumed());
            let offered = offered_flags();
            let socket = RecordingSocket::default();
            let (wire, sd_len) = offers_wire_bytes(&[(0x5678, 1, 1)]);
            let sd_view = sd::SdHeaderView::parse(&wire[..sd_len]).expect("parse");
            let subscriptions = std::sync::Arc::new(tokio::sync::RwLock::new(
                crate::server::SubscriptionManager::new(),
            ));

            handle_sd_message(
                &config,
                &socket,
                &make_sd_state(),
                &subscriptions,
                &sd_view,
                core::net::SocketAddr::V4(PEER_SD),
                &mut [0u8; crate::UDP_BUFFER_SIZE],
                &offered,
            )
            .await
            .expect("handle_sd_message");

            assert!(offered[0].load(core::sync::atomic::Ordering::Relaxed));
        }

        #[tokio::test]
        async fn run_keeps_receiving_after_the_find_arm_finishes() {
            use futures_util::FutureExt;
            let config = make_config()
                .with_consumed_eventgroup(consumed())
                .with_announce(false);
            let sd: &'static RecordingSocket = std::boxed::Box::leak(std::boxed::Box::default());
            let unicast: &'static RecordingSocket =
                std::boxed::Box::leak(std::boxed::Box::default());
            let sd_state: &'static SdStateManager =
                std::boxed::Box::leak(std::boxed::Box::new(SdStateManager::new()));
            let subscriptions = std::sync::Arc::new(tokio::sync::RwLock::new(
                crate::server::SubscriptionManager::new(),
            ));
            let (mut u, mut s, mut r) = ([0u8; 64], [0u8; 64], [0u8; 512]);
            let run = run_combined(
                config,
                unicast,
                sd,
                subscriptions,
                sd_state,
                crate::NoE2E,
                RecordingTimer::new(),
                false,
                &mut u,
                &mut s,
                &mut r,
                &mut [],
                None,
            );

            assert!(run.now_or_never().is_none(), "run must still be receiving");
            assert_eq!(
                sd.sent.lock().unwrap().len(),
                4,
                "the Find arm ran inside run"
            );
        }
    }
}
