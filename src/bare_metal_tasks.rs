//! Spawnable, executor-agnostic SOME/IP futures for bare-metal targets.
//!
//! A bare-metal firmware spawns these on its own executor and provides
//! only the I/O: a [`TransportSocket`] (lwIP/smoltcp/embassy-net), a
//! [`Timer`], an [`E2ERegistryHandle`], a [`SubscriptionHandle`], and a
//! dispatch `fn`. All SOME/IP encoding/parsing lives here and in
//! [`crate::sd_codec`] — the firmware constructs no SOME/IP bytes.
//!
//! Each future is a plain `async fn`: every generic parameter is an
//! input, so the returned future captures exactly those and is `'static`
//! when the caller passes `'static` borrows (the pattern an
//! `#[embassy_executor::task]` body relies on). The server's inbound
//! path (SD subscribe-accept + request dispatch) is handled by
//! [`crate::server::Server::run_with_buffers`] and is not duplicated
//! here; these cover the announce, the notification-only client, and the
//! synchronous publish.

use core::future::Future;
use core::net::SocketAddrV4;
use core::pin::pin;
use core::sync::atomic::AtomicU16;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use core::time::Duration;

use crate::E2ECheckStatus;
use crate::protocol::sd::RebootFlag;
use crate::sd_codec::{
    OfferServiceRequest, SubscribeEventgroupRequest, build_multi_offer_service_datagram,
    build_notification_datagram, build_subscribe_eventgroup_datagram, check_parsed_e2e,
    e2e_status_code, next_sd_session, parse_someip_datagram,
};
use crate::server::{Subscriber, SubscriptionHandle};
use crate::transport::{E2ERegistryHandle, Timer, TransportSocket};

/// Periodically multicast one combined `OfferService` SD datagram
/// carrying every entry in `offers` (each with its own endpoint option,
/// a single session stream), re-sent every `period`. `N` bounds the
/// number of entries packed into the datagram. `scratch` must hold the
/// encoded datagram.
pub async fn announce_offers_future<'a, S, Tm, const N: usize>(
    sd_socket: &'a S,
    timer: &'a Tm,
    offers: &'a [OfferServiceRequest],
    sd_multicast: SocketAddrV4,
    session: &'a AtomicU16,
    period: Duration,
    scratch: &'a mut [u8],
) where
    S: TransportSocket,
    Tm: Timer,
{
    loop {
        let s = next_sd_session(session);
        if let Ok(len) = build_multi_offer_service_datagram::<N>(scratch, offers, s) {
            let _ = sd_socket.send_to(&scratch[..len], sd_multicast).await;
        }
        timer.sleep(period).await;
    }
}

/// Periodically multicast a `SubscribeEventgroup` for `request` to the SD
/// endpoint, re-sent every `period`. Subscribing proactively (rather
/// than waiting for an `OfferService`) supports providers that don't
/// announce cyclically. `reboot` is carried in each datagram's SD flags.
// One SD-request shape spread across distinct scalars + borrows; bundling
// them into a struct would just move the argument list to the call site.
#[allow(clippy::too_many_arguments)]
pub async fn subscribe_announce_future<'a, S, Tm>(
    sd_socket: &'a S,
    timer: &'a Tm,
    request: SubscribeEventgroupRequest,
    sd_multicast: SocketAddrV4,
    session: &'a AtomicU16,
    reboot: RebootFlag,
    period: Duration,
    scratch: &'a mut [u8],
) where
    S: TransportSocket,
    Tm: Timer,
{
    loop {
        let s = next_sd_session(session);
        if let Ok(len) = build_subscribe_eventgroup_datagram(scratch, &request, s, reboot) {
            let _ = sd_socket.send_to(&scratch[..len], sd_multicast).await;
        }
        timer.sleep(period).await;
    }
}

/// Receive notifications on `rx_socket`, parse the SOME/IP header,
/// optionally run the E2E check in place, and hand the parsed
/// `(service_id, method_id, payload, e2e_status)` to `dispatch`. `buf`
/// is the receive scratch (sized to the max inbound datagram).
///
/// When `e2e_enabled` is false the payload is dispatched verbatim with
/// status `0`. When true, [`check_parsed_e2e`] looks up the profile by
/// the parsed `(service, method)` and strips/validates the E2E header.
pub async fn event_rx_dispatch_future<'a, S, R>(
    rx_socket: &'a S,
    e2e: &'a R,
    e2e_enabled: bool,
    dispatch: fn(
        ctx: usize,
        source: SocketAddrV4,
        service_id: u16,
        method_id: u16,
        payload: &[u8],
        e2e_status: u8,
    ),
    ctx: usize,
    buf: &'a mut [u8],
) where
    S: TransportSocket,
    R: E2ERegistryHandle,
{
    loop {
        let (n, source) = match rx_socket.recv_from(&mut *buf).await {
            Ok(d) => (d.bytes_received, d.source),
            Err(_) => continue,
        };
        let Some(parsed) = parse_someip_datagram(&buf[..n]) else {
            continue;
        };
        let (status, body) = if e2e_enabled {
            check_parsed_e2e(e2e, &parsed)
        } else {
            (E2ECheckStatus::Unchecked, parsed.payload)
        };
        dispatch(
            ctx,
            source,
            parsed.service_id,
            parsed.method_id,
            body,
            e2e_status_code(status),
        );
    }
}

/// Cap on `OfferService` entries packed into the combined announce
/// datagram inside [`run_someip`]. Covers any realistic catalog.
const RUN_OFFER_CAP: usize = 16;

/// Everything [`run_someip`] needs besides the server receive future:
/// the shared SD socket (for announce + subscribe), the client RX socket,
/// timer, E2E handle, and the per-future config/state/scratch. All borrows
/// share one lifetime; the scratch buffers must be distinct (no aliasing).
pub struct SomeipRun<'a, S, Tm, R>
where
    S: TransportSocket,
    Tm: Timer,
    R: E2ERegistryHandle,
{
    /// SD socket used to send offers and subscribes (send-only here).
    pub sd_socket: &'a S,
    /// Unicast socket the client receives notifications on.
    pub rx_socket: &'a S,
    pub timer: &'a Tm,
    pub e2e: &'a R,
    /// SD multicast endpoint (offers + subscribes are sent here).
    pub sd_multicast: SocketAddrV4,
    /// Re-announce / re-subscribe period (the node SD TTL).
    pub period: Duration,
    /// Offered services packed into one combined `OfferService`.
    pub offers: &'a [OfferServiceRequest],
    pub offer_session: &'a AtomicU16,
    pub offer_scratch: &'a mut [u8],
    /// `Some` to run the notification-only client (subscribe + RX);
    /// `None` for a provider-only node.
    pub subscribe: Option<SubscribeEventgroupRequest>,
    pub sub_session: &'a AtomicU16,
    pub sub_scratch: &'a mut [u8],
    pub sub_reboot: RebootFlag,
    /// One-shot delay before the first subscribe, to phase it off the
    /// offer announce (both run at `period`).
    pub sub_offset: Duration,
    pub sub_e2e_enabled: bool,
    pub rx_buf: &'a mut [u8],
    pub dispatch: fn(
        ctx: usize,
        source: SocketAddrV4,
        service_id: u16,
        method_id: u16,
        payload: &[u8],
        e2e_status: u8,
    ),
    /// Opaque context word forwarded verbatim as `dispatch`'s first arg.
    /// The reusable runtime passes `0` (its trampoline injects the real
    /// ctx); a direct caller passes its own.
    pub dispatch_ctx: usize,
}

/// Drive the full bare-metal SOME/IP integration as ONE future: the
/// caller-supplied server receive future (`recv`, typically
/// `Server::run_with_buffers`) concurrently with the combined-offer
/// announce, the proactive subscribe (offset off the announce), and the
/// notification RX+dispatch. Spawn this from a single
/// `#[embassy_executor::task]` so the firmware holds no per-future task
/// glue.
///
/// `recv` is taken as an opaque `Future` so this stays generic over just
/// `S/Tm/R` instead of the receive server's full bound set. The future
/// never resolves (every branch loops forever); spawn and forget.
pub async fn run_someip<S, Tm, R, RecvFut>(recv: RecvFut, cfg: SomeipRun<'_, S, Tm, R>)
where
    S: TransportSocket,
    Tm: Timer,
    R: E2ERegistryHandle,
    RecvFut: Future,
{
    let SomeipRun {
        sd_socket,
        rx_socket,
        timer,
        e2e,
        sd_multicast,
        period,
        offers,
        offer_session,
        offer_scratch,
        subscribe,
        sub_session,
        sub_scratch,
        sub_reboot,
        sub_offset,
        sub_e2e_enabled,
        rx_buf,
        dispatch,
        dispatch_ctx,
    } = cfg;

    let announce = announce_offers_future::<_, _, RUN_OFFER_CAP>(
        sd_socket,
        timer,
        offers,
        sd_multicast,
        offer_session,
        period,
        offer_scratch,
    );

    // Subscribe (phased off the announce) + RX run only for a consumer
    // node; for a provider-only node they idle forever so the join arity
    // stays fixed.
    let subscribe_fut = async move {
        if let Some(request) = subscribe {
            timer.sleep(sub_offset).await;
            subscribe_announce_future(
                sd_socket,
                timer,
                request,
                sd_multicast,
                sub_session,
                sub_reboot,
                period,
                sub_scratch,
            )
            .await;
        } else {
            core::future::pending::<()>().await;
        }
    };
    let rx_fut = async move {
        if subscribe.is_some() {
            event_rx_dispatch_future(
                rx_socket,
                e2e,
                sub_e2e_enabled,
                dispatch,
                dispatch_ctx,
                rx_buf,
            )
            .await;
        } else {
            core::future::pending::<()>().await;
        }
    };

    // All four loop forever; the join never resolves.
    futures_util::join!(recv, announce, subscribe_fut, rx_fut);
}

// No-op waker to drive the synchronous `for_each_subscriber` future in
// `publish_notification` to completion. `StaticSubscriptionHandle`
// resolves it on the first poll (its body is a synchronous storage
// read), so one poll suffices.
const NOOP_RAW_WAKER: RawWaker = {
    const VTABLE: RawWakerVTable = RawWakerVTable::new(|_| NOOP_RAW_WAKER, |_| {}, |_| {}, |_| {});
    RawWaker::new(core::ptr::null(), &VTABLE)
};

/// Build a SOME/IP notification for `(service, method)` carrying
/// `payload` into `scratch` and transmit it to every current subscriber
/// of `(service, instance, event_group)` via the caller-supplied `send`
/// closure. Synchronous — intended for a firmware's publish FFI.
///
/// `send(datagram, subscriber_addr)` performs the actual UDP transmit
/// (the firmware's lwIP send). `scratch` is the caller's static TX
/// buffer (no per-call stack buffer). Returns the number of subscribers
/// sent to (`>= 0`), or a negative error: `-2` if `scratch` is too small
/// for the header + payload.
// A SOME/IP notification's full addressing (service/instance/eg/method/
// session) plus payload, scratch, and send sink; a params struct would
// only relocate the list.
#[allow(clippy::too_many_arguments)]
pub fn publish_notification<Sub, FSend>(
    subscriptions: &Sub,
    service_id: u16,
    instance_id: u16,
    event_group_id: u16,
    method_id: u16,
    session: u16,
    payload: &[u8],
    scratch: &mut [u8],
    mut send: FSend,
) -> i32
where
    Sub: SubscriptionHandle,
    FSend: FnMut(&[u8], SocketAddrV4),
{
    let Ok(total) = build_notification_datagram(scratch, service_id, method_id, session, payload)
    else {
        return -2;
    };
    let datagram = &scratch[..total];

    let send_one = |sub: &Subscriber| send(datagram, sub.address);
    // `for_each_subscriber` is synchronous-backed on the bare-metal
    // handle and resolves to the subscriber count on the first poll;
    // drive it once under a no-op waker.
    let fut = subscriptions.for_each_subscriber(service_id, instance_id, event_group_id, send_one);
    let mut fut = pin!(fut);
    // SAFETY: NOOP_RAW_WAKER's vtable functions are all no-ops / return
    // the same waker, satisfying the RawWaker contract.
    let waker = unsafe { Waker::from_raw(NOOP_RAW_WAKER) };
    let mut cx = Context::from_waker(&waker);
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(n) => n as i32,
        Poll::Pending => 0,
    }
}
