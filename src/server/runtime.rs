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

use futures_util::future::FusedFuture;
use futures_util::{FutureExt, future::Either, pin_mut, select_biased};

use crate::protocol::sd;
use crate::transport::{E2EHandle, SharedHandle, TransportSocket};
use crate::{ServiceInstance, Timer};

use super::discovery::{Discovered, Discovery};
use super::sd_state::SdStateManager;
use super::subscription_manager::{SubscribeError, SubscriptionHandle};
use super::{Error, ServerConfig};

/// Send a unicast `OfferService` to a specific address (typically in
/// response to a `FindService`).
///
/// `buf` is a caller-provided scratch buffer used for encoding the outgoing
/// frame. Returns [`Error::Capacity`]`("udp_buffer")` if the encoded frame
/// does not fit in `buf`, a protocol error if `offering.ttl` is outside
/// 1..=[`MAX_TTL`](sd::MAX_TTL), or the socket's transport error.
pub(super) async fn send_unicast_offer<T>(
    buf: &mut [u8],
    config: &ServerConfig,
    offering: &super::Offering,
    sd_socket: &T,
    sd_state: &SdStateManager,
    target: core::net::SocketAddr,
) -> Result<(), Error>
where
    T: TransportSocket,
{
    let target_v4 = socket_addr_v4(target)?;
    let total_len = sd_state.encode_offer(buf, offering, config.interface)?;
    sd_socket.send_to(&buf[..total_len], target_v4).await?;
    crate::log::debug!(
        "Sent unicast OfferService to {} for service 0x{:04X}",
        target,
        offering.service.service_id()
    );

    Ok(())
}

/// Send `peer` the `SubscribeAckEventGroup` answering `entry_view`, with
/// `ttl`; a TTL of 0 makes it a `SubscribeNack`. `buf` is the encoding
/// scratch.
async fn send_subscribe_answer<T>(
    buf: &mut [u8],
    sd_socket: &T,
    sd_state: &SdStateManager,
    entry_view: &sd::EntryView<'_>,
    ttl: u32,
    peer: core::net::SocketAddr,
) -> Result<(), Error>
where
    T: TransportSocket,
{
    let request = crate::sd_codec::SubscribeAckRequest {
        service_id: entry_view.service_id(),
        instance_id: entry_view.instance_id(),
        event_group_id: entry_view.event_group_id(),
        major_version: entry_view.major_version(),
        ttl,
        counter: crate::sd_codec::SubscribeCounter::of_entry(entry_view),
    };
    let peer = socket_addr_v4(peer)?;
    let len = crate::sd_codec::build_subscribe_ack_datagram(buf, &request, sd_state.next_session())
        .map_err(super::build_error)?;
    sd_socket.send_to(&buf[..len], peer).await?;
    Ok(())
}

/// Send `SubscribeAck` for `entry_view` to `subscriber`.
///
/// `buf` is a caller-provided scratch buffer used for encoding the outgoing
/// frame. Returns [`Error::Capacity`]`("udp_buffer")` if the encoded frame
/// does not fit in `buf`.
pub(super) async fn send_subscribe_ack_from_view<T>(
    buf: &mut [u8],
    offering: &super::Offering,
    sd_socket: &T,
    sd_state: &SdStateManager,
    entry_view: &sd::EntryView<'_>,
    subscriber: core::net::SocketAddr,
) -> Result<(), Error>
where
    T: TransportSocket,
{
    send_subscribe_answer(
        buf,
        sd_socket,
        sd_state,
        entry_view,
        offering.ttl,
        subscriber,
    )
    .await?;
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
    send_subscribe_answer(buf, sd_socket, sd_state, entry_view, 0, subscriber).await?;
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
    crate::sd_codec::build_multi_subscribe_eventgroup_datagram::<
        { ServerConfig::CONSUMED_EVENTGROUPS_CAP },
    >(buf, &requests, sd_state.next_session())
    .map_err(super::build_error)
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

/// Handle a Service Discovery message (Subscribe / `FindService` etc.),
/// recording the offers and stop offers it carries in `discovery`.
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
    discovery: &mut Discovery<'_>,
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

                let offering = accepting_offering(config.offering.as_ref(), entry_view);
                if let Err(reason) = offering {
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
                } else if let Ok(offering) = offering
                    && let Some(endpoint_addr) = extract_subscriber_endpoint(entry_options)
                {
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
                                offering,
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
                if let Some(offering) = &config.offering
                    && (find_service_id == offering.service.service_id()
                        || find_service_id == 0xFFFF)
                {
                    crate::log::debug!(
                        "Received FindService from {} for service 0x{:04X} (ours: 0x{:04X}), sending unicast offer",
                        sender,
                        find_service_id,
                        offering.service.service_id()
                    );
                    if let Err(e) =
                        send_unicast_offer(send_buf, config, offering, sd_socket, sd_state, sender)
                            .await
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
            sd::EntryType::OfferService => {
                let Some(instance) = entry_instance(entry_view) else {
                    continue;
                };
                let stop = entry_view.is_stop_offer();
                let holds = if stop {
                    discovery.stop(sender, instance)
                } else {
                    let endpoint = offered_endpoint(entry_options);
                    discovery.offer(sender, instance, endpoint, entry_view.ttl())
                };
                if !holds {
                    continue;
                }
                let requests = config.consumed_eventgroups.iter().zip(&mut answered);
                for (_, subscribe) in requests.filter(|(request, _)| request.service == instance) {
                    *subscribe = !stop;
                }
            }
            _ => {
                crate::log::trace!("Ignoring SD entry type: {:?}", entry_type);
            }
        }
    }

    if answered.contains(&true) {
        match send_subscribes(send_buf, config, sd_socket, sd_state, &answered, sender).await {
            Ok(()) => {
                let requests = config.consumed_eventgroups.iter().zip(answered);
                for (request, _) in requests.filter(|(_, answered)| *answered) {
                    discovery.subscribed(sender, request.service);
                }
            }
            Err(e) => crate::log::warn!("Subscribe send failed: {e}"),
        }
    }
    Ok(())
}

/// The service instance an SD entry names; `None` when it names a wildcard.
fn entry_instance(entry: sd::EntryView<'_>) -> Option<ServiceInstance> {
    ServiceInstance::new(
        entry.service_id(),
        entry.instance_id(),
        entry.major_version(),
    )
}

/// The offering that accepts the Subscribe `entry`: `offering` itself, or one
/// of its [`accepted_offers`](super::Offering::accepted_offers). `Err` holds
/// the reason for the `SubscribeNack`.
fn accepting_offering<'a>(
    offering: Option<&'a super::Offering>,
    entry: sd::EntryView<'_>,
) -> Result<&'a super::Offering, &'static str> {
    let Some(offering) = offering else {
        crate::log::warn!(
            "Subscribe for service 0x{:04X} on a server that offers nothing",
            entry.service_id()
        );
        return Err("service_not_offered");
    };
    if entry_instance(entry)
        .is_some_and(|instance| offering.accepts_offer(instance, entry.event_group_id()))
    {
        return Ok(offering);
    }
    let ours = offering.service;
    if entry.service_id() != ours.service_id() {
        crate::log::warn!(
            "Subscribe for wrong service: expected 0x{:04X}, got 0x{:04X}",
            ours.service_id(),
            entry.service_id()
        );
        Err("wrong_service_id")
    } else if entry.instance_id() != ours.instance_id() {
        crate::log::warn!(
            "Subscribe for wrong instance: expected {}, got {}",
            ours.instance_id(),
            entry.instance_id()
        );
        Err("wrong_instance_id")
    } else if entry.major_version() != ours.major_version() {
        crate::log::warn!(
            "Subscribe for wrong major_version: expected {}, got {}",
            ours.major_version(),
            entry.major_version()
        );
        Err("wrong_major_version")
    } else if !offering.accepts_event_group(entry.event_group_id()) {
        crate::log::warn!(
            "Subscribe for unknown event_group_id 0x{:04X} (service 0x{:04X})",
            entry.event_group_id(),
            entry.service_id()
        );
        Err("unknown_event_group")
    } else {
        Ok(offering)
    }
}

/// The endpoint an `OfferService` entry names among `options`, the options it
/// references: see [`preferred_ipv4_endpoint`](crate::net_endpoint::preferred_ipv4_endpoint).
fn offered_endpoint<'a>(
    options: impl Iterator<Item = sd::OptionView<'a>>,
) -> Option<crate::NetEndpoint> {
    crate::net_endpoint::preferred_ipv4_endpoint(
        options
            .filter(|option| matches!(option.option_type(), Ok(sd::OptionType::IpV4Endpoint)))
            .filter_map(|option| option.as_ipv4().ok()),
    )
}

/// Periodic SD `OfferService` announcement loop. Runs forever; intended
/// to be combined with the receive loop via [`run_combined`].
pub(super) async fn announce_loop<T, Tm>(
    config: &ServerConfig,
    offering: &super::Offering,
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
            .send_offer_service(announce_send_buf, config, offering, sd_socket)
            .await
        {
            Ok(()) => {
                announcement_count += 1;
                if announcement_count == 1 {
                    crate::log::info!(
                        "Sent first SD announcement for service 0x{:04X}",
                        offering.service.service_id()
                    );
                } else {
                    crate::log::debug!(
                        "Sent {} SD announcements for service 0x{:04X}",
                        announcement_count,
                        offering.service.service_id()
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
    let parsed = crate::sd_codec::ParsedMessage::from(view);
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

/// What the receive loop answers the messages it receives with.
struct Receiver<'a, T, Sub, R> {
    config: &'a ServerConfig,
    unicast_socket: &'a T,
    sd_socket: &'a T,
    sd_state: &'a SdStateManager,
    subscriptions: &'a Sub,
    e2e: &'a R,
    non_sd_observer: Option<(super::NonSdRequestCallback, usize)>,
}

/// Handles one message received from `addr`: SD messages go to
/// [`handle_sd_message`], non-SD unicast requests to
/// [`dispatch_non_sd_request`].
async fn handle_message<T, Sub, R>(
    receiver: &Receiver<'_, T, Sub, R>,
    discovery: &mut Discovery<'_>,
    send_buf: &mut [u8],
    view: &crate::protocol::MessageView<'_>,
    addr: core::net::SocketAddr,
    from_unicast: bool,
) -> Result<(), Error>
where
    T: TransportSocket,
    Sub: SubscriptionHandle,
    R: E2EHandle + Clone,
{
    let Receiver {
        config,
        unicast_socket,
        sd_socket,
        sd_state,
        subscriptions,
        e2e,
        non_sd_observer,
    } = *receiver;
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
                    discovery,
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
                dispatch_non_sd_request(unicast_socket, observer, e2e, view, src_v4, send_buf)
                    .await;
            }
        } else {
            crate::log::trace!("Non-SD unicast SOME/IP message, no observer registered — ignoring");
        }
    } else {
        crate::log::trace!("Non-SD multicast SOME/IP message, ignoring");
    }
    Ok(())
}

/// Receive loop body — drives `recv_from` on both the unicast and SD
/// sockets and hands each message in a datagram to [`handle_message`].
async fn recv_loop<T, Sub, R, Tm>(
    receiver: &Receiver<'_, T, Sub, R>,
    discovery: &mut Discovery<'_>,
    timer: &Tm,
    unicast_buf: &mut [u8],
    sd_buf: &mut [u8],
    send_buf: &mut [u8],
) -> Result<(), Error>
where
    T: TransportSocket,
    Sub: SubscriptionHandle,
    R: E2EHandle + Clone,
    Tm: Timer,
{
    let (unicast_socket, sd_socket) = (receiver.unicast_socket, receiver.sd_socket);
    // Runs while an offer can run out; kept across iterations so a steady
    // stream of datagrams does not restart it.
    let tick = futures_util::future::Fuse::<Tm::SleepFuture<'_>>::terminated();
    pin_mut!(tick);

    // Iteration counter used to flip `select_biased!` arm priority
    // each turn. We can't use the pseudo-random `select!` (it needs
    // `std`), so flipping arm order each iteration approximates the
    // fairness it would give without pulling std — a sustained
    // one-sided load (only-unicast or only-sd) cannot starve the
    // other arm.
    let mut prefer_sd_first = false;
    loop {
        if tick.is_terminated() && discovery.ticking() {
            tick.set(timer.sleep(super::discovery::TICK).fuse());
        }
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
        let datagram = {
            let unicast_fut = unicast_socket.recv_from(&mut *unicast_buf).fuse();
            let sd_fut = sd_socket.recv_from(&mut *sd_buf).fuse();
            pin_mut!(unicast_fut, sd_fut);
            if prefer_sd_first {
                select_biased! {
                    () = tick.as_mut() => None,
                    result = sd_fut => Some((result?, false)),
                    result = unicast_fut => Some((result?, true)),
                }
            } else {
                select_biased! {
                    () = tick.as_mut() => None,
                    result = unicast_fut => Some((result?, true)),
                    result = sd_fut => Some((result?, false)),
                }
            }
        };
        let Some((datagram, from_unicast)) = datagram else {
            discovery.tick();
            continue;
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

        let mut walk = crate::protocol::messages(data);
        for message in walk.by_ref() {
            match message {
                Ok(view) => {
                    handle_message(receiver, discovery, send_buf, &view, addr, from_unicast)
                        .await?;
                }
                Err(malformed) => malformed.log(addr),
            }
        }
        walk.log_discarded(addr);
    }
}

/// Encode one SD message with a `FindService` entry for each instance
/// `discovered` has not found; `None` when it has found every one.
fn encode_finds(
    buf: &mut [u8],
    discovered: &Discovered,
    sd_state: &SdStateManager,
) -> Result<Option<usize>, Error> {
    let wanted = discovered.unfound();
    if wanted.clone().next().is_none() {
        return Ok(None);
    }
    crate::sd_codec::build_find_service_datagram::<{ super::discovery::FIND_TARGETS_CAP }>(
        buf,
        wanted,
        sd_state.next_session(),
    )
    .map(Some)
    .map_err(super::build_error)
}

/// Send `FindService` for the instances `discovered` has not found, on
/// [`ServerConfig::find_timing`]'s schedule, and again on that schedule each
/// time it loses one. Never returns.
pub(super) async fn find_loop<T, Tm>(
    config: &ServerConfig,
    sd_socket: &T,
    sd_state: &SdStateManager,
    timer: &Tm,
    discovered: &Discovered,
    buf: &mut [u8; ServerConfig::FIND_SEND_BUF_LEN],
) -> core::convert::Infallible
where
    T: TransportSocket,
    Tm: Timer,
{
    loop {
        discovered.forget_losses();
        find_schedule(config, sd_socket, sd_state, timer, discovered, buf).await;
        discovered.lost().await;
    }
}

/// Send `FindService` for the instances `discovered` has not found on
/// [`ServerConfig::find_timing`]'s schedule, encoded into `buf`; returns
/// after the last send, or once every instance is found.
async fn find_schedule<T, Tm>(
    config: &ServerConfig,
    sd_socket: &T,
    sd_state: &SdStateManager,
    timer: &Tm,
    discovered: &Discovered,
    buf: &mut [u8; ServerConfig::FIND_SEND_BUF_LEN],
) where
    T: TransportSocket,
    Tm: Timer,
{
    let timing = config.find_timing;
    let mut delay = timing.initial_delay(super::find_timing::seed(config));
    for repetition in 0..=u32::from(timing.repetitions_max) {
        timer.sleep(delay).await;
        match encode_finds(buf, discovered, sd_state) {
            Ok(Some(len)) => {
                if let Err(e) = sd_socket.send_to(&buf[..len], config.sd_multicast).await {
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
/// Returns `Err(Error::InvalidUsage("passive_server_run"))` if invoked on a
/// passive server (passive servers have no SD socket bound to 30490 and rely
/// on an external SD dispatcher).
///
/// When the config offers no service, or its [`Offering::announce`](super::Offering::announce)
/// is `false`, the announcement arm is skipped and only
/// the receive loop drives. The dispatcher topology uses this, where a
/// co-located `Client` emits `OfferService` on the server's behalf.
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
    find_send_buf: &mut [u8; ServerConfig::FIND_SEND_BUF_LEN],
    non_sd_observer: Option<(super::NonSdRequestCallback, usize)>,
    service_observer: Option<(super::ServiceObserver, usize)>,
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
            "run called on passive Server; \
             SD receive must be driven externally (e.g. via the \
             Client's discovery socket, routing Subscribes to \
             `EventPublisher::register_subscriber`)"
        );
        return Err(Error::InvalidUsage("passive_server_run"));
    }

    let unicast = unicast_socket.get();
    let sd = sd_socket.get();
    let sd_state_ref = sd_state.get();

    let discovered = Discovered::new(&config);
    let mut discovery = Discovery::new(&discovered, service_observer);
    let receiver = Receiver {
        config: &config,
        unicast_socket: unicast,
        sd_socket: sd,
        sd_state: sd_state_ref,
        subscriptions: &subscriptions,
        e2e: &e2e,
        non_sd_observer,
    };
    let recv_fut = recv_loop(
        &receiver,
        &mut discovery,
        &timer,
        unicast_buf,
        sd_buf,
        recv_send_buf,
    );
    let announce_fut = async {
        if let Some(offering) = config
            .offering
            .as_ref()
            .filter(|offering| offering.announce)
        {
            announce_loop(
                &config,
                offering,
                sd,
                sd_state_ref,
                &timer,
                announce_send_buf,
            )
            .await;
        }
        core::future::pending::<core::convert::Infallible>().await
    };
    let find_fut = async {
        if discovered.is_empty() {
            core::future::pending::<core::convert::Infallible>().await
        } else {
            find_loop(
                &config,
                sd,
                sd_state_ref,
                &timer,
                &discovered,
                find_send_buf,
            )
            .await
        }
    };
    let senders = futures_util::future::join(announce_fut, find_fut);
    pin_mut!(recv_fut, senders);
    match futures_util::future::select(recv_fut, senders).await {
        Either::Left((recv_result, _)) => recv_result,
        Either::Right(((never, _), _)) => match never {},
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
    use crate::CapacityKind;
    use crate::server::Offering;

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
        ServerConfig::new(
            Offering::new(ServiceInstance::new(0x1234, 1, 1).unwrap()).with_local_port(30500),
        )
        .with_interface(Ipv4Addr::LOCALHOST)
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

        let result = send_unicast_offer(
            &mut [0u8; 24],
            &config,
            config.test_offering(),
            &socket,
            &sd_state,
            target,
        )
        .await;

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

        let result = send_unicast_offer(
            &mut [0u8; 8],
            &config,
            config.test_offering(),
            &socket,
            &sd_state,
            target,
        )
        .await;

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
            config.test_offering(),
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
            counter: 3,
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
            config.test_offering(),
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
            config.test_offering(),
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
        use crate::ServiceInstance;
        use crate::server::FindTiming;

        /// Records every datagram sent through it.
        #[derive(Default)]
        pub(super) struct RecordingSocket {
            pub(super) sent: std::sync::Mutex<std::vec::Vec<(std::vec::Vec<u8>, SocketAddrV4)>>,
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

        pub(super) const PEER_SD: SocketAddrV4 =
            SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 2), 30490);

        fn consumed() -> crate::sd_codec::SubscribeEventgroupRequest {
            crate::sd_codec::SubscribeEventgroupRequest {
                service: ServiceInstance::new(0x5678, 1, 1).unwrap(),
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
                &mut Discovery::leaked(config),
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
        /// [`offers_wire_bytes`] with no option, so no offer names an endpoint.
        fn offers_without_endpoint_wire_bytes(offers: &[(u16, u16, u8)]) -> ([u8; 512], usize) {
            use automotive_wire_codec::Encode;

            let entries: std::vec::Vec<_> = offers
                .iter()
                .map(|&(service_id, instance_id, major_version)| {
                    sd::Entry::OfferService(sd::ServiceEntry {
                        index_first_options_run: 0,
                        index_second_options_run: 0,
                        options_count: sd::OptionsCount::new(0, 0),
                        service_id,
                        instance_id,
                        major_version,
                        ttl: 3,
                        minor_version: 0,
                    })
                })
                .collect();
            let sd_payload = sd::Header::new(
                sd::Flags::new_sd(sd::RebootFlag::RecentlyRebooted),
                &entries,
                &[],
            );
            let mut wire = [0u8; 512];
            let sd_len = sd_payload.encode_to_slice(&mut wire).expect("encode");
            (wire, sd_len)
        }

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
                &mut Discovery::leaked(config),
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
        #[cfg_attr(
            feature = "bare_metal",
            ignore = "consumes several eventgroups; bare_metal allows one by default"
        )]
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
        #[cfg_attr(
            feature = "bare_metal",
            ignore = "consumes several eventgroups; bare_metal allows one by default"
        )]
        async fn only_the_offered_services_are_subscribed() {
            let other = crate::sd_codec::SubscribeEventgroupRequest {
                service: ServiceInstance::new(0x5679, 1, 1).unwrap(),
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
            let config =
                ServerConfig::new(Offering::new(ServiceInstance::new(0x9999, 1, 1).unwrap()));
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
                &mut Discovery::leaked(&config),
            )
            .await;

            assert!(result.is_ok(), "{result:?}");
        }

        /// Returns at once from every `sleep`, recording its duration; on
        /// the sleep numbered `offer_at` (0-based) it marks the first
        /// instance found.
        struct RecordingTimer<'a> {
            sleeps: std::sync::Mutex<std::vec::Vec<core::time::Duration>>,
            offer_at: Option<(usize, &'a Discovered)>,
        }

        impl<'a> RecordingTimer<'a> {
            fn new() -> Self {
                Self {
                    sleeps: std::sync::Mutex::default(),
                    offer_at: None,
                }
            }
            fn offering_at(n: usize, discovered: &'a Discovered) -> Self {
                Self {
                    sleeps: std::sync::Mutex::default(),
                    offer_at: Some((n, discovered)),
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
                if let Some((n, discovered)) = self.offer_at
                    && sleeps.len() == n
                {
                    discovered.mark_found(0);
                }
                sleeps.push(duration);
                core::future::ready(())
            }
        }

        fn find_buf() -> [u8; ServerConfig::FIND_SEND_BUF_LEN] {
            [0; ServerConfig::FIND_SEND_BUF_LEN]
        }

        fn required() -> ServiceInstance {
            ServiceInstance::new(0x6001, 2, 3).unwrap()
        }

        /// The (service, instance) of each Find entry in each datagram sent.
        fn finds(socket: &RecordingSocket) -> std::vec::Vec<std::vec::Vec<(u16, u16)>> {
            socket
                .sent
                .lock()
                .unwrap()
                .iter()
                .map(|(d, to)| {
                    assert_eq!(*to, SocketAddrV4::new(sd::MULTICAST_IP, sd::MULTICAST_PORT));
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

        /// [`FindTiming::DEFAULT`] with a fixed 10 ms initial delay.
        const FIXED: FindTiming = FindTiming {
            initial_delay_min: core::time::Duration::from_millis(10),
            initial_delay_max: core::time::Duration::from_millis(10),
            ..FindTiming::DEFAULT
        };

        fn once() -> FindTiming {
            FindTiming {
                repetitions_max: 0,
                ..FIXED
            }
        }

        /// Runs one find schedule for `config` on `socket`, with nothing
        /// found; returns the timer.
        async fn schedule(
            config: &ServerConfig,
            socket: &RecordingSocket,
        ) -> RecordingTimer<'static> {
            let timer = RecordingTimer::new();
            find_schedule(
                config,
                socket,
                &make_sd_state(),
                &timer,
                &Discovered::new(config),
                &mut find_buf(),
            )
            .await;
            timer
        }

        #[tokio::test]
        async fn finds_after_the_initial_delay_then_doubling_repetitions() {
            let config = make_config()
                .with_consumed_eventgroup(consumed())
                .with_find_timing(FIXED);
            let socket = RecordingSocket::default();

            let timer = schedule(&config, &socket).await;

            assert_eq!(timer.millis(), [10, 100, 200, 400]);
            assert_eq!(finds(&socket), std::vec![std::vec![(0x5678, 1)]; 4]);
        }

        #[tokio::test]
        async fn the_first_find_waits_the_initial_delay_drawn_for_the_node() {
            let config = make_config().with_consumed_eventgroup(consumed());

            let timer = schedule(&config, &RecordingSocket::default()).await;

            let first = timer.sleeps.lock().unwrap()[0];
            let timing = FindTiming::DEFAULT;
            assert!((timing.initial_delay_min..=timing.initial_delay_max).contains(&first));
            let seed = crate::server::find_timing::seed(&config);
            assert_eq!(first, timing.initial_delay(seed));
        }

        #[tokio::test]
        async fn zero_repetitions_send_one_find() {
            let config = make_config()
                .with_consumed_eventgroup(consumed())
                .with_find_timing(once());
            let socket = RecordingSocket::default();

            let timer = schedule(&config, &socket).await;

            assert_eq!(timer.millis(), [10]);
            assert_eq!(finds(&socket).len(), 1);
        }

        #[tokio::test]
        async fn an_instance_offered_before_the_first_send_is_never_found() {
            let config = make_config().with_consumed_eventgroup(consumed());
            let discovered = Discovered::new(&config);
            let (socket, timer) = (
                RecordingSocket::default(),
                RecordingTimer::offering_at(0, &discovered),
            );

            find_schedule(
                &config,
                &socket,
                &make_sd_state(),
                &timer,
                &discovered,
                &mut find_buf(),
            )
            .await;

            assert!(finds(&socket).is_empty());
        }

        #[tokio::test]
        async fn an_offer_between_repetitions_stops_the_remaining_finds() {
            let config = make_config().with_consumed_eventgroup(consumed());
            let discovered = Discovered::new(&config);
            let (socket, timer) = (
                RecordingSocket::default(),
                RecordingTimer::offering_at(2, &discovered),
            );

            find_schedule(
                &config,
                &socket,
                &make_sd_state(),
                &timer,
                &discovered,
                &mut find_buf(),
            )
            .await;

            assert_eq!(finds(&socket).len(), 2);
        }

        #[tokio::test]
        async fn finding_starts_over_when_a_found_instance_is_lost() {
            let config = make_config()
                .with_consumed_eventgroup(consumed())
                .with_find_timing(FIXED);
            let discovered = Discovered::new(&config);
            let (socket, timer, sd_state) = (
                RecordingSocket::default(),
                RecordingTimer::offering_at(1, &discovered),
                make_sd_state(),
            );
            let mut buf = find_buf();
            let finding = find_loop(&config, &socket, &sd_state, &timer, &discovered, &mut buf);
            pin_mut!(finding);

            assert!(futures_util::poll!(finding.as_mut()).is_pending());
            assert_eq!(finds(&socket).len(), 1, "found after the first Find");
            discovered.mark_lost(0);
            assert!(futures_util::poll!(finding.as_mut()).is_pending());

            assert_eq!(timer.millis(), [10, 100, 10, 100, 200, 400]);
            assert_eq!(finds(&socket).len(), 5);
        }

        #[tokio::test]
        #[cfg_attr(
            feature = "bare_metal",
            ignore = "consumes several eventgroups; bare_metal allows one by default"
        )]
        async fn eventgroups_of_one_instance_share_one_find_entry() {
            let second = crate::sd_codec::SubscribeEventgroupRequest {
                event_group_id: 0x0002,
                local_rx_port: 40101,
                ..consumed()
            };
            let other = crate::sd_codec::SubscribeEventgroupRequest {
                service: ServiceInstance::new(0x5679, 1, 1).unwrap(),
                ..consumed()
            };
            let config = make_config()
                .with_consumed_eventgroup(consumed())
                .with_consumed_eventgroup(second)
                .with_consumed_eventgroup(other)
                .with_find_timing(once());
            let socket = RecordingSocket::default();

            schedule(&config, &socket).await;

            assert_eq!(finds(&socket), [std::vec![(0x5678, 1), (0x5679, 1)]]);
        }

        #[tokio::test]
        async fn finds_a_required_service_that_is_not_subscribed() {
            let config = ServerConfig::consumer_only()
                .with_required_service(required())
                .with_find_timing(once());
            let socket = RecordingSocket::default();

            schedule(&config, &socket).await;

            assert_eq!(finds(&socket), [std::vec![(0x6001, 2)]]);
            let sent = socket.sent.lock().unwrap();
            let view = crate::protocol::MessageView::parse(&sent[0].0).expect("SOME/IP");
            let sd_view = view.sd_header().expect("SD");
            let e = sd_view.entries().next().expect("one entry");
            assert_eq!(e.major_version(), 3);
            assert_eq!(e.ttl(), 3);
        }

        #[tokio::test]
        async fn a_required_service_also_consumed_is_found_once() {
            let config = make_config()
                .with_consumed_eventgroup(consumed())
                .with_required_service(consumed().service)
                .with_find_timing(once());
            let socket = RecordingSocket::default();

            schedule(&config, &socket).await;

            assert_eq!(finds(&socket), [std::vec![(0x5678, 1)]]);
        }

        #[tokio::test]
        async fn consumed_instances_are_found_before_required_ones() {
            let config = make_config()
                .with_required_service(required())
                .with_consumed_eventgroup(consumed())
                .with_find_timing(once());
            let socket = RecordingSocket::default();

            schedule(&config, &socket).await;

            assert_eq!(finds(&socket), [std::vec![(0x5678, 1), (0x6001, 2)]]);
        }

        /// The Finds one schedule sends after `discovery` handled `offers`
        /// from [`PEER_SD`]; `with_endpoint` decides whether each offer
        /// names an IPv4 endpoint.
        async fn finds_after_offers(
            config: &ServerConfig,
            offers: &[(u16, u16, u8)],
            with_endpoint: bool,
        ) -> std::vec::Vec<std::vec::Vec<(u16, u16)>> {
            let discovered = Discovered::new(config);
            let socket = RecordingSocket::default();
            let (wire, sd_len) = if with_endpoint {
                offers_wire_bytes(offers)
            } else {
                offers_without_endpoint_wire_bytes(offers)
            };
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
                &mut Discovery::new(&discovered, None),
            )
            .await
            .expect("handle_sd_message");

            let find_socket = RecordingSocket::default();
            find_schedule(
                config,
                &find_socket,
                &make_sd_state(),
                &RecordingTimer::new(),
                &discovered,
                &mut find_buf(),
            )
            .await;
            finds(&find_socket)
        }

        #[tokio::test]
        async fn an_offer_of_a_required_service_stops_its_finds() {
            let config = make_config()
                .with_consumed_eventgroup(consumed())
                .with_required_service(required());

            let finds = finds_after_offers(&config, &[(0x6001, 2, 3)], true).await;

            assert_eq!(finds, std::vec![std::vec![(0x5678, 1)]; 4]);
        }

        #[tokio::test]
        async fn an_offer_with_no_endpoint_does_not_stop_the_finds() {
            let config = ServerConfig::consumer_only()
                .with_required_service(required())
                .with_find_timing(once());

            let finds = finds_after_offers(&config, &[(0x6001, 2, 3)], false).await;

            assert_eq!(finds, [std::vec![(0x6001, 2)]]);
        }

        #[tokio::test]
        async fn an_answered_offer_stops_the_finds_of_its_eventgroup() {
            let config = make_config().with_consumed_eventgroup(consumed());

            let finds = finds_after_offers(&config, &[(0x5678, 1, 1)], true).await;

            assert!(finds.is_empty(), "{finds:?}");
        }

        #[tokio::test]
        async fn an_offer_whose_subscribe_send_fails_keeps_finding_its_eventgroup() {
            let config = make_config()
                .with_consumed_eventgroup(consumed())
                .with_find_timing(once());
            let discovered = Discovered::new(&config);
            let (wire, sd_len) = offers_wire_bytes(&[(0x5678, 1, 1)]);
            let sd_view = sd::SdHeaderView::parse(&wire[..sd_len]).expect("parse");
            let subscriptions = std::sync::Arc::new(tokio::sync::RwLock::new(
                crate::server::SubscriptionManager::new(),
            ));
            handle_sd_message(
                &config,
                &FailingSocket,
                &make_sd_state(),
                &subscriptions,
                &sd_view,
                core::net::SocketAddr::V4(PEER_SD),
                &mut [0u8; crate::UDP_BUFFER_SIZE],
                &mut Discovery::new(&discovered, None),
            )
            .await
            .expect("handle_sd_message");

            assert_eq!(discovered.unfound().count(), 1);
        }

        /// How many Subscribes `discovery`'s server sends answering an SD
        /// message of `entries` and `options` from `sender`.
        async fn subscribes_answering(
            config: &ServerConfig,
            discovery: &mut Discovery<'_>,
            sender: SocketAddrV4,
            entries: &[sd::Entry],
            options: &[sd::Options],
        ) -> usize {
            use automotive_wire_codec::Encode;
            let header = sd::Header::new(
                sd::Flags::new_sd(sd::RebootFlag::RecentlyRebooted),
                entries,
                options,
            );
            let mut wire = [0u8; 512];
            let sd_len = header.encode_to_slice(&mut wire).expect("encode");
            let sd_view = sd::SdHeaderView::parse(&wire[..sd_len]).expect("parse");
            let socket = RecordingSocket::default();
            let subscriptions = std::sync::Arc::new(tokio::sync::RwLock::new(
                crate::server::SubscriptionManager::new(),
            ));
            handle_sd_message(
                config,
                &socket,
                &make_sd_state(),
                &subscriptions,
                &sd_view,
                core::net::SocketAddr::V4(sender),
                &mut [0u8; crate::UDP_BUFFER_SIZE],
                discovery,
            )
            .await
            .expect("handle_sd_message");
            socket.sent.lock().unwrap().len()
        }

        fn consumed_offer(ttl: u32, options: u8) -> sd::Entry {
            sd::Entry::OfferService(sd::ServiceEntry {
                index_first_options_run: 0,
                index_second_options_run: 0,
                options_count: sd::OptionsCount::new(options, 0),
                service_id: 0x5678,
                instance_id: 1,
                major_version: 1,
                ttl,
                minor_version: 0,
            })
        }

        fn consumed_stop() -> sd::Entry {
            let sd::Entry::OfferService(entry) = consumed_offer(0, 1) else {
                unreachable!()
            };
            sd::Entry::StopOfferService(entry)
        }

        fn endpoint() -> sd::Options {
            sd::Options::IpV4Endpoint {
                ip: *PEER_SD.ip(),
                port: 30509,
                protocol: sd::TransportProtocol::Udp,
            }
        }

        #[tokio::test]
        async fn only_the_holders_offer_is_answered_and_found() {
            let config = make_config().with_consumed_eventgroup(consumed());
            let discovered = Discovered::new(&config);
            let mut discovery = Discovery::new(&discovered, None);
            let other = SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 3), 30490);

            let holder = subscribes_answering(
                &config,
                &mut discovery,
                PEER_SD,
                &[consumed_offer(3, 1)],
                &[endpoint()],
            )
            .await;
            assert_eq!(holder, 1);
            let second = subscribes_answering(
                &config,
                &mut discovery,
                other,
                &[consumed_offer(3, 1)],
                &[endpoint()],
            )
            .await;

            assert_eq!(second, 0, "a second provider gets no Subscribe");
        }

        #[tokio::test]
        async fn an_offer_with_no_endpoint_is_not_answered() {
            let config = make_config().with_consumed_eventgroup(consumed());
            let discovered = Discovered::new(&config);

            let sent = subscribes_answering(
                &config,
                &mut Discovery::new(&discovered, None),
                PEER_SD,
                &[consumed_offer(3, 0)],
                &[],
            )
            .await;

            assert_eq!(sent, 0);
            assert_eq!(discovered.unfound().count(), 1);
        }

        #[tokio::test]
        async fn an_offer_stopped_in_the_same_message_is_not_answered() {
            let config = make_config().with_consumed_eventgroup(consumed());
            let discovered = Discovered::new(&config);

            let sent = subscribes_answering(
                &config,
                &mut Discovery::new(&discovered, None),
                PEER_SD,
                &[consumed_offer(3, 1), consumed_stop()],
                &[endpoint()],
            )
            .await;

            assert_eq!(sent, 0);
            assert_eq!(discovered.unfound().count(), 1);
        }

        #[tokio::test]
        async fn find_entries_carry_the_consumed_ttl_and_major_version() {
            let config = make_config()
                .with_consumed_eventgroup(consumed())
                .with_find_timing(once());
            let socket = RecordingSocket::default();

            schedule(&config, &socket).await;

            let sent = socket.sent.lock().unwrap();
            let view = crate::protocol::MessageView::parse(&sent[0].0).expect("SOME/IP");
            let sd_view = view.sd_header().expect("SD");
            let e = sd_view.entries().next().expect("one entry");
            assert_eq!((e.major_version(), e.ttl()), (1, 5));
            assert_eq!(sd_view.options().count(), 0);
        }

        #[tokio::test]
        async fn a_failed_find_send_keeps_the_schedule() {
            let config = make_config()
                .with_consumed_eventgroup(consumed())
                .with_find_timing(FIXED);
            let timer = RecordingTimer::new();

            find_schedule(
                &config,
                &FailingSocket,
                &make_sd_state(),
                &timer,
                &Discovered::new(&config),
                &mut find_buf(),
            )
            .await;

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

            let timer = schedule(&config, &RecordingSocket::default()).await;

            let sleeps = timer.sleeps.lock().unwrap();
            assert_eq!(sleeps.len(), 256);
            assert!(sleeps[1..].iter().all(|d| *d == core::time::Duration::MAX));
        }

        #[test]
        fn a_found_instance_is_no_longer_found() {
            let config = make_config().with_consumed_eventgroup(consumed());
            let discovered = Discovered::new(&config);
            discovered.mark_found(0);

            let found = encode_finds(&mut [0u8; 64], &discovered, &make_sd_state());

            assert!(matches!(found, Ok(None)), "{found:?}");
        }

        #[tokio::test]
        async fn finds_and_offers_go_to_the_configured_sd_multicast() {
            let group = SocketAddrV4::new(Ipv4Addr::new(239, 1, 2, 3), 30500);
            let config = make_config()
                .with_consumed_eventgroup(consumed())
                .with_find_timing(once())
                .with_sd_multicast(group);
            let (socket, sd_state) = (RecordingSocket::default(), make_sd_state());

            find_schedule(
                &config,
                &socket,
                &sd_state,
                &RecordingTimer::new(),
                &Discovered::new(&config),
                &mut find_buf(),
            )
            .await;
            sd_state
                .send_offer_service(&mut [0u8; 64], &config, config.test_offering(), &socket)
                .await
                .expect("offer");

            let sent = socket.sent.lock().unwrap();
            let destinations: std::vec::Vec<_> = sent.iter().map(|(_, to)| *to).collect();
            assert_eq!(destinations, [group, group]);
        }

        #[tokio::test]
        async fn run_keeps_receiving_after_the_find_arm_finishes() {
            let mut config = make_config().with_consumed_eventgroup(consumed());
            config.offering.as_mut().unwrap().announce = false;
            assert_eq!(finds_sent_by_run(config), 4, "the Find arm ran inside run");
        }

        #[tokio::test]
        async fn run_finds_required_services_with_no_consumed_eventgroup() {
            let config = ServerConfig::consumer_only().with_required_service(required());
            assert_eq!(finds_sent_by_run(config), 4);
        }

        /// Polls `run_combined` once and returns how many datagrams it sent to
        /// the SD socket; the run future must still be receiving.
        fn finds_sent_by_run(config: ServerConfig) -> usize {
            use futures_util::FutureExt;
            let sd: &'static RecordingSocket = std::boxed::Box::leak(std::boxed::Box::default());
            let unicast: &'static RecordingSocket =
                std::boxed::Box::leak(std::boxed::Box::default());
            let sd_state: &'static SdStateManager =
                std::boxed::Box::leak(std::boxed::Box::new(SdStateManager::new()));
            let subscriptions = std::sync::Arc::new(tokio::sync::RwLock::new(
                crate::server::SubscriptionManager::new(),
            ));
            let (mut u, mut s, mut r) = ([0u8; 64], [0u8; 64], [0u8; 512]);
            let mut f = find_buf();
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
                &mut f,
                None,
                None,
            );

            assert!(run.now_or_never().is_none(), "run must still be receiving");
            let sent = sd.sent.lock().unwrap().len();
            sent
        }

        /// The datagram the codec builds for the answer to
        /// `subscribe_wire_bytes`' Subscribe with `ttl`.
        fn codec_answer(ttl: u32) -> std::vec::Vec<u8> {
            let request = crate::sd_codec::SubscribeAckRequest {
                service_id: 0x1234,
                instance_id: 1,
                event_group_id: 0x0001,
                major_version: 1,
                ttl,
                counter: crate::sd_codec::SubscribeCounter::new(3).unwrap(),
            };
            let mut buf = [0u8; 64];
            let len = crate::sd_codec::build_subscribe_ack_datagram(
                &mut buf,
                &request,
                crate::sd_codec::SdSessionCounter::new().next(),
            )
            .expect("build");
            buf[..len].to_vec()
        }

        #[tokio::test]
        async fn the_server_ack_and_nack_are_the_codec_answers() {
            let config = make_config();
            let (wire, sd_len) = subscribe_wire_bytes();
            let sd_view = sd::SdHeaderView::parse(&wire[..sd_len]).expect("parse");
            let entry_view = sd_view.entries().next().expect("one entry");

            let socket = RecordingSocket::default();
            send_subscribe_ack_from_view(
                &mut [0u8; crate::UDP_BUFFER_SIZE],
                config.test_offering(),
                &socket,
                &make_sd_state(),
                &entry_view,
                subscriber_addr(),
            )
            .await
            .expect("ack");
            send_subscribe_nack_from_view(
                &mut [0u8; crate::UDP_BUFFER_SIZE],
                &config,
                &socket,
                &make_sd_state(),
                &entry_view,
                subscriber_addr(),
                "test",
            )
            .await
            .expect("nack");

            let sent = socket.sent.lock().unwrap();
            assert_eq!(sent[0].0, codec_answer(config.test_offering().ttl), "ack");
            assert_eq!(sent[1].0, codec_answer(0), "nack");
        }

        /// The codec's `OfferService` for `config`, as the first SD message.
        fn codec_offer(config: &ServerConfig) -> std::vec::Vec<u8> {
            let mut buf = [0u8; 64];
            let len = crate::sd_codec::build_offer_service_datagram(
                &mut buf,
                &config.test_offering().offer_request(config.interface),
                crate::sd_codec::SdSessionCounter::new().next(),
            )
            .expect("build");
            buf[..len].to_vec()
        }

        #[tokio::test]
        async fn the_server_offers_are_the_codec_offer() {
            let mut config = make_config();
            config.offering.as_mut().unwrap().minor_version = 7;

            let unicast = RecordingSocket::default();
            send_unicast_offer(
                &mut [0u8; crate::UDP_BUFFER_SIZE],
                &config,
                config.test_offering(),
                &unicast,
                &make_sd_state(),
                subscriber_addr(),
            )
            .await
            .expect("unicast offer");
            let multicast = RecordingSocket::default();
            make_sd_state()
                .send_offer_service(
                    &mut [0u8; crate::UDP_BUFFER_SIZE],
                    &config,
                    config.test_offering(),
                    &multicast,
                )
                .await
                .expect("multicast offer");

            let unicast = unicast.sent.lock().unwrap();
            let multicast = multicast.sent.lock().unwrap();
            assert_eq!(unicast[0].0, codec_offer(&config), "unicast");
            assert_eq!(unicast[0].1, socket_addr_v4(subscriber_addr()).unwrap());
            assert_eq!(multicast[0].0, codec_offer(&config), "multicast");
            assert_eq!(
                multicast[0].1,
                SocketAddrV4::new(sd::MULTICAST_IP, sd::MULTICAST_PORT)
            );
        }
    }

    #[cfg(feature = "server-tokio")]
    mod service_observer {
        use super::consumed_eventgroups::{PEER_SD, RecordingSocket};
        use super::*;
        use crate::server::ServiceChange;
        use crate::{NetEndpoint, TransportProtocol};
        use std::sync::Mutex;

        static SEEN: Mutex<std::vec::Vec<(usize, ServiceChange)>> =
            Mutex::new(std::vec::Vec::new());

        fn record(ctx: usize, change: ServiceChange) {
            SEEN.lock().unwrap().push((ctx, change));
        }

        /// What the observer registered under `ctx` has seen, in call order.
        fn seen(ctx: usize) -> std::vec::Vec<ServiceChange> {
            SEEN.lock()
                .unwrap()
                .iter()
                .filter(|(c, _)| *c == ctx)
                .map(|&(_, change)| change)
                .collect()
        }

        fn service(service_id: u16, first_option: u8, options: u8, ttl: u32) -> sd::ServiceEntry {
            sd::ServiceEntry {
                index_first_options_run: first_option,
                index_second_options_run: 0,
                options_count: sd::OptionsCount::new(options, 0),
                service_id,
                instance_id: 3,
                major_version: 2,
                ttl,
                minor_version: 7,
            }
        }

        fn endpoint_option(port: u16, protocol: sd::TransportProtocol) -> sd::Options {
            sd::Options::IpV4Endpoint {
                ip: Ipv4Addr::new(10, 1, 2, 3),
                port,
                protocol,
            }
        }

        fn instance(service_id: u16) -> ServiceInstance {
            ServiceInstance::new(service_id, 3, 2).unwrap()
        }

        fn udp(port: u16) -> NetEndpoint {
            NetEndpoint::udp(core::net::SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::new(10, 1, 2, 3),
                port,
            )))
        }

        fn available(service_id: u16, endpoint: NetEndpoint) -> ServiceChange {
            ServiceChange::Available {
                service: instance(service_id),
                endpoint,
            }
        }

        /// A discovery of services 0x6001 and 0x6002 reporting to the observer
        /// registered under `ctx`.
        fn discovery(ctx: usize) -> Discovery<'static> {
            let config = ServerConfig::consumer_only()
                .with_required_service(instance(0x6001))
                .with_required_service(instance(0x6002));
            let discovered = std::boxed::Box::leak(std::boxed::Box::new(Discovered::new(&config)));
            Discovery::new(discovered, Some((record, ctx)))
        }

        /// Delivers an SD message of `entries` and `options` from
        /// [`PEER_SD`] to a server with `discovery`.
        async fn deliver(
            discovery: &mut Discovery<'_>,
            entries: &[sd::Entry],
            options: &[sd::Options],
        ) {
            use automotive_wire_codec::Encode;

            let header = sd::Header::new(
                sd::Flags::new_sd(sd::RebootFlag::RecentlyRebooted),
                entries,
                options,
            );
            let mut wire = [0u8; 512];
            let sd_len = header.encode_to_slice(&mut wire).expect("encode");
            let sd_view = sd::SdHeaderView::parse(&wire[..sd_len]).expect("parse");
            let subscriptions = std::sync::Arc::new(tokio::sync::RwLock::new(
                crate::server::SubscriptionManager::new(),
            ));
            handle_sd_message(
                &make_config(),
                &RecordingSocket::default(),
                &make_sd_state(),
                &subscriptions,
                &sd_view,
                core::net::SocketAddr::V4(PEER_SD),
                &mut [0u8; crate::UDP_BUFFER_SIZE],
                discovery,
            )
            .await
            .expect("handle_sd_message");
        }

        #[tokio::test]
        async fn each_offer_is_available_at_its_own_endpoint() {
            deliver(
                &mut discovery(0x4F01),
                &[
                    sd::Entry::OfferService(service(0x6001, 0, 1, 30)),
                    sd::Entry::OfferService(service(0x6002, 1, 1, 90)),
                ],
                &[
                    endpoint_option(30501, sd::TransportProtocol::Udp),
                    endpoint_option(30502, sd::TransportProtocol::Udp),
                ],
            )
            .await;

            assert_eq!(
                seen(0x4F01),
                [available(0x6001, udp(30501)), available(0x6002, udp(30502))]
            );
        }

        #[tokio::test]
        async fn a_stop_offer_of_the_holder_is_gone() {
            let mut discovery = discovery(0x4F02);
            let options = [endpoint_option(30501, sd::TransportProtocol::Udp)];
            deliver(
                &mut discovery,
                &[sd::Entry::OfferService(service(0x6001, 0, 1, 30))],
                &options,
            )
            .await;
            deliver(
                &mut discovery,
                &[sd::Entry::StopOfferService(service(0x6001, 0, 1, 0))],
                &options,
            )
            .await;

            assert_eq!(
                seen(0x4F02),
                [
                    available(0x6001, udp(30501)),
                    ServiceChange::Gone {
                        service: instance(0x6001)
                    }
                ]
            );
        }

        #[tokio::test]
        async fn an_offer_without_an_endpoint_reports_nothing() {
            deliver(
                &mut discovery(0x4F03),
                &[sd::Entry::OfferService(service(0x6001, 0, 0, 30))],
                &[],
            )
            .await;

            assert!(seen(0x4F03).is_empty());
        }

        #[tokio::test]
        async fn a_udp_endpoint_is_preferred_over_an_earlier_tcp_one() {
            let mut entry = service(0x6001, 0, 2, 30);
            entry.options_count = sd::OptionsCount::new(2, 0);
            deliver(
                &mut discovery(0x4F05),
                &[sd::Entry::OfferService(entry)],
                &[
                    endpoint_option(30501, sd::TransportProtocol::Tcp),
                    endpoint_option(30502, sd::TransportProtocol::Udp),
                ],
            )
            .await;

            assert!(matches!(
                seen(0x4F05)[..],
                [ServiceChange::Available {
                    endpoint: NetEndpoint {
                        protocol: TransportProtocol::Udp,
                        ..
                    },
                    ..
                }]
            ));
        }

        /// An IPv4 SD endpoint option (0x24) naming UDP port 30490.
        fn sd_endpoint_option() -> sd::Options {
            sd::Options::IpV4SD {
                ip: Ipv4Addr::new(10, 1, 2, 3),
                protocol: sd::TransportProtocol::Udp,
                port: 30490,
            }
        }

        #[tokio::test]
        async fn an_sd_endpoint_option_is_not_the_service_endpoint() {
            deliver(
                &mut discovery(0x4F06),
                &[sd::Entry::OfferService(service(0x6001, 0, 2, 30))],
                &[
                    sd_endpoint_option(),
                    endpoint_option(30502, sd::TransportProtocol::Udp),
                ],
            )
            .await;

            assert_eq!(seen(0x4F06), [available(0x6001, udp(30502))]);
        }

        #[tokio::test]
        async fn an_sd_endpoint_option_alone_reports_nothing() {
            deliver(
                &mut discovery(0x4F07),
                &[sd::Entry::OfferService(service(0x6001, 0, 1, 30))],
                &[sd_endpoint_option()],
            )
            .await;

            assert!(seen(0x4F07).is_empty());
        }

        #[tokio::test]
        async fn other_entries_report_nothing() {
            deliver(
                &mut discovery(0x4F04),
                &[
                    sd::Entry::FindService(sd::ServiceEntry::find(0x6001)),
                    sd::Entry::SubscribeEventGroup(sd::EventGroupEntry {
                        index_first_options_run: 0,
                        index_second_options_run: 0,
                        options_count: sd::OptionsCount::new(1, 0),
                        service_id: 0x1234,
                        instance_id: 1,
                        major_version: 1,
                        ttl: 3,
                        counter: 0,
                        event_group_id: 0x0001,
                    }),
                ],
                &[endpoint_option(40000, sd::TransportProtocol::Udp)],
            )
            .await;

            assert!(seen(0x4F04).is_empty());
        }

        /// A socket whose first receive is one SD message offering 0x6001
        /// for `ttl` seconds from [`PEER_SD`], and whose later receives
        /// never complete.
        struct OneOffer {
            ttl: u32,
            delivered: core::sync::atomic::AtomicBool,
        }

        impl TransportSocket for OneOffer {
            type SendFuture<'a> = NullSend;
            type RecvFuture<'a> = futures_util::future::Either<
                core::future::Ready<Result<ReceivedDatagram, TransportError>>,
                NullRecv,
            >;

            fn send_to<'a>(
                &'a self,
                _buf: &'a [u8],
                _target: SocketAddrV4,
            ) -> Self::SendFuture<'a> {
                NullSend
            }
            fn recv_from<'a>(&'a self, buf: &'a mut [u8]) -> Self::RecvFuture<'a> {
                use automotive_wire_codec::Encode;
                if self
                    .delivered
                    .swap(true, core::sync::atomic::Ordering::Relaxed)
                {
                    return futures_util::future::Either::Right(NullRecv);
                }
                let entries = [sd::Entry::OfferService(service(0x6001, 0, 1, self.ttl))];
                let options = [endpoint_option(30501, sd::TransportProtocol::Udp)];
                let header = sd::Header::new(
                    sd::Flags::new_sd(sd::RebootFlag::RecentlyRebooted),
                    &entries,
                    &options,
                );
                let mut sd_data = std::vec::Vec::new();
                header.encode(&mut sd_data).expect("encode");
                let mut bytes = std::vec::Vec::new();
                crate::protocol::Header::new_sd(1, sd_data.len())
                    .encode(&mut bytes)
                    .expect("encode");
                bytes.extend_from_slice(&sd_data);
                let len = bytes.len();
                buf[..len].copy_from_slice(&bytes);
                futures_util::future::Either::Left(core::future::ready(Ok(ReceivedDatagram {
                    bytes_received: len,
                    source: PEER_SD,
                    truncated: false,
                })))
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

        /// Sleeps that complete at once, counted.
        #[derive(Default)]
        struct CountingTimer(core::sync::atomic::AtomicUsize);

        impl crate::Timer for CountingTimer {
            type SleepFuture<'a>
                = core::future::Ready<()>
            where
                Self: 'a;
            fn sleep(&self, _duration: core::time::Duration) -> Self::SleepFuture<'_> {
                self.0.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                core::future::ready(())
            }
        }

        /// What the receive loop reports, and how many ticks it slept, after
        /// it receives an offer of 0x6001 for `ttl` seconds.
        fn receive_one_offer(ctx: usize, ttl: u32) -> (std::vec::Vec<ServiceChange>, usize) {
            use futures_util::FutureExt;
            let config = make_config().with_required_service(instance(0x6001));
            let discovered = Discovered::new(&config);
            let mut discovery = Discovery::new(&discovered, Some((record, ctx)));
            let socket = OneOffer {
                ttl,
                delivered: core::sync::atomic::AtomicBool::new(false),
            };
            let subscriptions = std::sync::Arc::new(tokio::sync::RwLock::new(
                crate::server::SubscriptionManager::new(),
            ));
            let receiver = Receiver {
                config: &config,
                unicast_socket: &socket,
                sd_socket: &socket,
                sd_state: &make_sd_state(),
                subscriptions: &subscriptions,
                e2e: &crate::NoE2E,
                non_sd_observer: None,
            };
            let timer = CountingTimer::default();
            let (mut u, mut s, mut r) = ([0u8; 512], [0u8; 512], [0u8; 512]);
            let receiving = recv_loop(&receiver, &mut discovery, &timer, &mut u, &mut s, &mut r);

            assert!(
                receiving.now_or_never().is_none(),
                "the loop keeps receiving"
            );
            (seen(ctx), timer.0.into_inner())
        }

        #[test]
        fn the_receive_loop_reports_an_offer_gone_once_its_ttl_runs_out() {
            let (changes, ticks) = receive_one_offer(0x4F08, 3);

            assert_eq!(
                changes,
                [
                    available(0x6001, udp(30501)),
                    ServiceChange::Gone {
                        service: instance(0x6001)
                    }
                ]
            );
            assert_eq!(ticks, 4, "between TTL and TTL + 1 s");
        }

        #[test]
        fn an_offer_for_the_maximum_ttl_never_runs_out() {
            let (changes, ticks) = receive_one_offer(0x4F09, sd::MAX_TTL);

            assert_eq!(changes, [available(0x6001, udp(30501))]);
            assert_eq!(ticks, 0);
        }
    }

    #[cfg(feature = "server-tokio")]
    mod consumer_only {
        use super::consumed_eventgroups::{PEER_SD, RecordingSocket};
        use super::*;

        /// An SD payload holding `entry` and one IPv4 endpoint option.
        fn wire_bytes(entry: sd::Entry) -> ([u8; 512], usize) {
            use automotive_wire_codec::Encode;

            let entries = [entry];
            let options = [sd::Options::IpV4Endpoint {
                ip: Ipv4Addr::LOCALHOST,
                port: 40000,
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

        fn find_wire_bytes(service_id: u16) -> ([u8; 512], usize) {
            wire_bytes(sd::Entry::FindService(sd::ServiceEntry::find(service_id)))
        }

        fn subscribe_to(service_id: u16, instance_id: u16) -> ([u8; 512], usize) {
            wire_bytes(sd::Entry::SubscribeEventGroup(sd::EventGroupEntry {
                index_first_options_run: 0,
                index_second_options_run: 0,
                options_count: sd::OptionsCount::new(1, 0),
                service_id,
                instance_id,
                major_version: 1,
                ttl: 3,
                counter: 0,
                event_group_id: 0x0001,
            }))
        }

        /// The datagrams `config` sends in answer to the SD message `wire`.
        async fn answer(
            config: &ServerConfig,
            &(wire, sd_len): &([u8; 512], usize),
        ) -> RecordingSocket {
            let socket = RecordingSocket::default();
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
                &mut Discovery::leaked(config),
            )
            .await
            .expect("handle_sd_message");
            socket
        }

        #[tokio::test]
        async fn consumer_only_answers_no_find_service() {
            let config = ServerConfig::consumer_only();

            for service_id in [0x0000, 0xFFFF] {
                let socket = answer(&config, &find_wire_bytes(service_id)).await;
                assert!(
                    socket.sent.lock().unwrap().is_empty(),
                    "FindService for 0x{service_id:04X} is answered"
                );
            }
        }

        #[tokio::test]
        async fn an_offering_server_still_answers_find_service() {
            let config = make_config();

            let socket = answer(&config, &find_wire_bytes(0xFFFF)).await;

            assert_eq!(socket.sent.lock().unwrap().len(), 1);
        }

        #[tokio::test]
        async fn consumer_only_nacks_a_subscribe() {
            let config = ServerConfig::consumer_only();

            let socket = answer(&config, &subscribe_to(0x0000, 0)).await;

            let sent = socket.sent.lock().unwrap();
            assert_eq!(sent.len(), 1, "one SubscribeNack");
            let view = crate::protocol::MessageView::parse(&sent[0].0).expect("SOME/IP");
            let entry = view
                .sd_header()
                .expect("SD")
                .entries()
                .next()
                .expect("one entry");
            assert!(matches!(
                entry.entry_type(),
                Ok(sd::EntryType::SubscribeAck)
            ));
            assert_eq!(entry.ttl(), 0, "a Nack carries TTL 0");
        }

        #[test]
        fn an_offering_ttl_outside_the_entry_range_is_rejected() {
            let mut config =
                ServerConfig::new(Offering::new(ServiceInstance::new(0x1234, 1, 1).unwrap()));
            config.offering.as_mut().unwrap().ttl = 0;
            assert!(config.validate().is_err());
        }

        #[test]
        fn consumer_only_still_checks_the_subscribe_ttl() {
            let request = crate::sd_codec::SubscribeEventgroupRequest {
                service: ServiceInstance::new(0x5678, 1, 1).unwrap(),
                event_group_id: 0x0001,
                ttl: 0,
                local_ip: Ipv4Addr::LOCALHOST,
                local_rx_port: 40100,
            };
            let config = ServerConfig::consumer_only().with_consumed_eventgroup(request);

            assert!(config.validate().is_err());
        }

        /// A timer whose first sleep never completes.
        struct PendingTimer;

        impl crate::Timer for PendingTimer {
            type SleepFuture<'a>
                = core::future::Pending<()>
            where
                Self: 'a;
            fn sleep(&self, _duration: core::time::Duration) -> Self::SleepFuture<'_> {
                core::future::pending()
            }
        }

        #[test]
        fn consumer_only_announces_nothing() {
            let config = ServerConfig::consumer_only();
            let sd: &'static RecordingSocket = std::boxed::Box::leak(std::boxed::Box::default());
            let unicast: &'static RecordingSocket =
                std::boxed::Box::leak(std::boxed::Box::default());
            let sd_state: &'static SdStateManager =
                std::boxed::Box::leak(std::boxed::Box::new(SdStateManager::new()));
            let subscriptions = std::sync::Arc::new(tokio::sync::RwLock::new(
                crate::server::SubscriptionManager::new(),
            ));
            let (mut u, mut s, mut r, mut a, mut f) = (
                [0u8; 64],
                [0u8; 64],
                [0u8; 512],
                [0u8; crate::UDP_BUFFER_SIZE],
                [0u8; ServerConfig::FIND_SEND_BUF_LEN],
            );
            let run = run_combined(
                config,
                unicast,
                sd,
                subscriptions,
                sd_state,
                crate::NoE2E,
                PendingTimer,
                false,
                &mut u,
                &mut s,
                &mut r,
                &mut a,
                &mut f,
                None,
                None,
            );

            assert!(futures_util::FutureExt::now_or_never(run).is_none());
            assert!(sd.sent.lock().unwrap().is_empty());
        }
    }
}
