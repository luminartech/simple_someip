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

use core::net::SocketAddrV4;

use futures_util::{FutureExt, future::Either, pin_mut, select_biased};

use crate::Timer;
use crate::protocol::sd::{self, Entry, Flags, OptionsCount, ServiceEntry, TransportProtocol};
use crate::transport::{E2ERegistryHandle, SharedHandle, TransportSocket};

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
    let (sid, reboot_flag) = sd_state.next_session_id_with_reboot_flag();
    let sd_payload = sd::Header::new(Flags::new_sd(reboot_flag), &entries, &options);

    // Guard: SOME/IP header needs 16 bytes; SD payload needs the rest.
    if buf.len() < 16 {
        return Err(Error::Capacity("udp_buffer"));
    }
    let sd_data_len = sd_payload
        .encode_to_slice(&mut buf[16..])
        .map_err(|_| Error::Capacity("udp_buffer"))?;
    let total_len = 16 + sd_data_len;
    // The `< 16` guard plus `encode_to_slice`'s own over-capacity error
    // already cover the fit; this stays as a debug-only sanity check
    // rather than a live (dead) branch.
    debug_assert!(total_len <= buf.len());
    let someip_header = SomeIpHeader::new_sd(sid, sd_data_len);
    someip_header
        .encode_to_slice(&mut buf[..16])
        .map_err(|_| Error::Capacity("udp_buffer"))?;

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
    use crate::traits::WireFormat;

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
        return Err(Error::Capacity("udp_buffer"));
    }
    let sd_data_len = sd_payload
        .encode_to_slice(&mut buf[16..])
        .map_err(|_| Error::Capacity("udp_buffer"))?;
    let total_len = 16 + sd_data_len;
    // The `< 16` guard plus `encode_to_slice`'s own over-capacity error
    // already cover the fit; this stays as a debug-only sanity check
    // rather than a live (dead) branch.
    debug_assert!(total_len <= buf.len());
    let someip_header = SomeIpHeader::new_sd(sid, sd_data_len);
    someip_header
        .encode_to_slice(&mut buf[..16])
        .map_err(|_| Error::Capacity("udp_buffer"))?;

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
    use crate::traits::WireFormat;

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
        return Err(Error::Capacity("udp_buffer"));
    }
    let sd_data_len = sd_payload
        .encode_to_slice(&mut buf[16..])
        .map_err(|_| Error::Capacity("udp_buffer"))?;
    let total_len = 16 + sd_data_len;
    // The `< 16` guard plus `encode_to_slice`'s own over-capacity error
    // already cover the fit; this stays as a debug-only sanity check
    // rather than a live (dead) branch.
    debug_assert!(total_len <= buf.len());
    let someip_header = SomeIpHeader::new_sd(sid, sd_data_len);
    someip_header
        .encode_to_slice(&mut buf[..16])
        .map_err(|_| Error::Capacity("udp_buffer"))?;

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

/// Handle a Service Discovery message (Subscribe / `FindService` etc.).
#[allow(clippy::too_many_lines)]
pub(super) async fn handle_sd_message<T, Sub>(
    config: &ServerConfig,
    sd_socket: &T,
    sd_state: &SdStateManager,
    subscriptions: &Sub,
    sd_view: &sd::SdHeaderView<'_>,
    sender: core::net::SocketAddr,
    send_buf: &mut [u8],
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

    for entry_view in sd_view.entries() {
        let entry_type = entry_view.entry_type()?;
        match entry_type {
            sd::EntryType::Subscribe => {
                crate::log::debug!(
                    "Received Subscribe from {}: service=0x{:04X}, instance={}, eventgroup=0x{:04X}",
                    sender,
                    entry_view.service_id(),
                    entry_view.instance_id(),
                    entry_view.event_group_id()
                );

                // A co-offered `(service, instance, event_group)` registered
                // via `with_accepted_offer` is accepted on this shared recv
                // loop even though it is not the primary service — its tuple
                // is fully validated by the `accepts_offer` match, so the
                // four single-service guards below are skipped for it. Empty
                // `accepted_offers` ⇒ `co_offered` is always false ⇒ exact
                // single-service behaviour.
                let co_offered = config.accepts_offer(
                    entry_view.service_id(),
                    entry_view.instance_id(),
                    entry_view.event_group_id(),
                );

                if !co_offered && entry_view.service_id() != config.service_id {
                    crate::log::warn!(
                        "Subscribe for wrong service: expected 0x{:04X}, got 0x{:04X}",
                        config.service_id,
                        entry_view.service_id()
                    );
                    send_subscribe_nack_from_view(
                        send_buf,
                        config,
                        sd_socket,
                        sd_state,
                        &entry_view,
                        sender,
                        "wrong_service_id",
                    )
                    .await?;
                } else if !co_offered && entry_view.instance_id() != config.instance_id {
                    crate::log::warn!(
                        "Subscribe for wrong instance: expected {}, got {}",
                        config.instance_id,
                        entry_view.instance_id()
                    );
                    send_subscribe_nack_from_view(
                        send_buf,
                        config,
                        sd_socket,
                        sd_state,
                        &entry_view,
                        sender,
                        "wrong_instance_id",
                    )
                    .await?;
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
                } else {
                    let first_index = entry_view.index_first_options_run() as usize;
                    let first_count = entry_view.options_count().first_options_count as usize;
                    let second_index = entry_view.index_second_options_run() as usize;
                    let second_count = entry_view.options_count().second_options_count as usize;
                    if let Some(endpoint_addr) = extract_subscriber_endpoint(
                        &sd_view.options(),
                        first_index,
                        first_count,
                        second_index,
                        second_count,
                    ) {
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
            _ => {
                crate::log::trace!("Ignoring SD entry type: {:?}", entry_type);
            }
        }
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
async fn dispatch_non_sd_request<T: TransportSocket, R: E2ERegistryHandle>(
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
    // Run the same E2E check the notification path uses: a request whose
    // (service, method) has a registered profile is validated and its E2E
    // header stripped; one with no profile passes through unchecked (status
    // 0).
    let parsed = crate::sd_codec::ParsedDatagram {
        service_id,
        method_id,
        upper_header: hdr.upper_header_bytes(),
        payload: view.payload_bytes(),
    };
    let (status, body) = crate::sd_codec::check_parsed_e2e(e2e, &parsed);
    let resp_len = cb(
        ctx,
        source,
        service_id,
        method_id,
        body,
        crate::sd_codec::e2e_status_code(status),
        &mut send_buf[crate::sd_codec::SOMEIP_HEADER_LEN..],
    );
    // A negative length means "no response" (a setter / fire-and-forget).
    let Ok(payload_len) = usize::try_from(resp_len) else {
        return;
    };
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
) -> Result<(), Error>
where
    T: TransportSocket,
    Sub: SubscriptionHandle,
    R: E2ERegistryHandle,
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
    R: E2ERegistryHandle,
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
    );

    if config.announce {
        // Two DISTINCT send buffers: `recv_loop` and `announce_loop` run
        // concurrently under the `select` below, and both can be suspended
        // at a `send_to().await` simultaneously. Sharing one buffer would
        // mutably alias it across the two live futures — UB / corruption.
        let announce_fut = announce_loop(&config, sd, sd_state_ref, &timer, announce_send_buf);
        pin_mut!(recv_fut, announce_fut);
        match futures_util::future::select(recv_fut, announce_fut).await {
            Either::Left((recv_result, _)) => recv_result,
            Either::Right(((), recv_pending)) => recv_pending.await,
        }
    } else {
        recv_fut.await
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

pub(super) fn extract_subscriber_endpoint(
    options: &sd::OptionIter<'_>,
    first_index: usize,
    first_count: usize,
    second_index: usize,
    second_count: usize,
) -> Option<SocketAddrV4> {
    let mut first_endpoint: Option<SocketAddrV4> = None;
    let mut endpoint_count: usize = 0;
    let mut ignored_other: usize = 0;

    let mut walk_run = |index: usize, count: usize| {
        if count == 0 {
            return;
        }
        for option_view in options.clone().skip(index).take(count) {
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
    };

    walk_run(first_index, first_count);
    walk_run(second_index, second_count);

    match endpoint_count {
        0 => {
            crate::log::warn!(
                "No IPv4 endpoint in options runs \
                 (first: idx={first_index}, count={first_count}; \
                 second: idx={second_index}, count={second_count}; \
                 ignored={ignored_other})"
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
    /// not the SD payload) must return `Err(Capacity("udp_buffer"))`.
    #[tokio::test]
    async fn send_unicast_offer_undersized_buf_returns_capacity() {
        let config = make_config();
        let sd_state = make_sd_state();
        let socket = NullSocket;
        let target = subscriber_addr();

        let result = send_unicast_offer(&mut [0u8; 24], &config, &socket, &sd_state, target).await;

        assert!(
            matches!(result, Err(Error::Capacity("udp_buffer"))),
            "expected Capacity(\"udp_buffer\"), got {result:?}"
        );
    }

    /// `send_unicast_offer` with a buf shorter than the 16-byte SOME/IP
    /// header must return `Err(Capacity("udp_buffer"))`.
    #[tokio::test]
    async fn send_unicast_offer_buf_shorter_than_header_returns_capacity() {
        let config = make_config();
        let sd_state = make_sd_state();
        let socket = NullSocket;
        let target = subscriber_addr();

        let result = send_unicast_offer(&mut [0u8; 8], &config, &socket, &sd_state, target).await;

        assert!(
            matches!(result, Err(Error::Capacity("udp_buffer"))),
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
        use crate::traits::WireFormat;

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
    /// `Err(Capacity("udp_buffer"))` without panicking.
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
            matches!(result, Err(Error::Capacity("udp_buffer"))),
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
    /// `Err(Capacity("udp_buffer"))` without panicking.
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
            matches!(result, Err(Error::Capacity("udp_buffer"))),
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
}
