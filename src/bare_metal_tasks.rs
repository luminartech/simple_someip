//! Spawnable, executor-agnostic SOME/IP futures for bare-metal targets.
//!
//! A bare-metal firmware spawns these on its own executor and provides
//! only the I/O: a [`TransportSocket`] (lwIP/smoltcp/embassy-net), a
//! [`Timer`], an [`E2EHandle`], a [`SubscriptionHandle`], and a
//! dispatch `fn`. All SOME/IP encoding/parsing lives here and in
//! [`crate::sd_codec`] — the firmware constructs no SOME/IP bytes.
//!
//! Each future is a plain `async fn`: every generic parameter is an
//! input, so the returned future captures exactly those and is `'static`
//! when the caller passes `'static` borrows (the pattern an
//! `#[embassy_executor::task]` body relies on). The server's inbound
//! path (SD subscribe-accept + request dispatch) is handled by
//! [`crate::server::Server::run_with_buffers`] and is not duplicated
//! here; that path also finds the
//! [`crate::server::ServerConfig::consumed_eventgroups`] and answers their
//! Offers with Subscribes. These cover the announce, the notification
//! receive, and the synchronous publish.

use core::future::Future;
use core::net::SocketAddrV4;
use core::pin::pin;
use core::task::{Context, RawWaker, RawWakerVTable, Waker};
use core::time::Duration;

use crate::sd_codec::{
    OfferServiceRequest, build_multi_offer_service_datagram_with_reboot,
    build_notification_datagram, check_parsed_e2e, parse_someip_datagram,
};
use crate::server::{SdStateManager, ServerConfig, Subscriber, SubscriptionHandle};
use crate::transport::{E2EHandle, Timer, TransportSocket};
use heapless::Vec as HeaplessVec;

/// Handler for one decoded inbound request. A getter fills `response_out`
/// and returns the response length; a setter / fire-and-forget returns `<0`.
/// Args: `ctx`, sender, `(service, method, payload)`, E2E status,
/// `response_out`. Same signature as the runtime's `DispatchFn` and
/// [`crate::server::NonSdRequestCallback`]; kept as a local alias so this
/// module needs no `bare-metal-runtime` dependency.
pub type RequestDispatchFn = fn(
    ctx: usize,
    source: SocketAddrV4,
    service_id: u16,
    method_id: u16,
    payload: &[u8],
    e2e_status: Option<crate::E2EStatus>,
    response_out: &mut [u8],
) -> i32;

/// Periodically multicast one combined `OfferService` SD datagram
/// carrying every entry in `offers` (each with its own endpoint option),
/// re-sent every `period`. Each datagram takes its session ID and reboot
/// flag from `sd_state`, the server's SD state, so the node's multicast
/// SD messages form one session stream. `N` bounds the
/// number of entries packed into the datagram. `scratch` must hold the
/// encoded datagram.
pub async fn announce_offers_future<'a, S, Tm, const N: usize>(
    sd_socket: &'a S,
    timer: &'a Tm,
    offers: &'a [OfferServiceRequest],
    sd_multicast: SocketAddrV4,
    sd_state: &'a SdStateManager,
    period: Duration,
    scratch: &'a mut [u8],
) where
    S: TransportSocket,
    Tm: Timer,
{
    loop {
        let (session, reboot) = sd_state.next_session();
        if let Ok(len) =
            build_multi_offer_service_datagram_with_reboot::<N>(scratch, offers, session, reboot)
        {
            let _ = sd_socket.send_to(&scratch[..len], sd_multicast).await;
        }
        timer.sleep(period).await;
    }
}

/// Receive notifications on `rx_socket`, parse the SOME/IP header, run
/// `e2e`'s check, and hand the parsed `(service_id, method_id, payload,
/// e2e_status)` to `dispatch`. `e2e` decides per message ID whether a
/// check applies; `e2e_status` is `None` when none does. `buf` is the receive
/// scratch (sized to the max inbound datagram).
pub async fn event_rx_dispatch_future<'a, S, R>(
    rx_socket: &'a S,
    e2e: &'a R,
    dispatch: RequestDispatchFn,
    ctx: usize,
    buf: &'a mut [u8],
) where
    S: TransportSocket,
    R: E2EHandle + ?Sized,
{
    loop {
        let (n, source) = match rx_socket.recv_from(&mut *buf).await {
            Ok(d) => (d.bytes_received, d.source),
            Err(_) => continue,
        };
        let Ok(parsed) = parse_someip_datagram(&buf[..n]) else {
            continue;
        };
        let (status, body) = check_parsed_e2e(e2e, core::net::IpAddr::V4(*source.ip()), &parsed);
        // Notifications received on the client RX socket are events, not
        // requests — there is no reply, so pass an empty response buffer and
        // ignore the (always-negative) return.
        let _ = dispatch(
            ctx,
            source,
            parsed.service_id,
            parsed.method_id,
            body,
            status,
            &mut [],
        );
    }
}

/// Cap on `OfferService` entries packed into the combined announce
/// datagram inside [`run_someip`].
const RUN_OFFER_CAP: usize = crate::from_env_or(option_env!("SIMPLE_SOMEIP_MAX_OFFERS"), 4);

/// Everything [`run_someip`] needs besides the server receive future:
/// the shared SD socket (for the announce), the client RX socket, timer,
/// E2E handle, and the per-future config/state/scratch. All borrows share
/// one lifetime; the scratch buffers must be distinct (no aliasing).
pub struct SomeipRun<'a, S, Tm, R>
where
    S: TransportSocket,
    Tm: Timer,
    R: E2EHandle + Clone,
{
    /// SD socket used to send offers (send-only here).
    pub sd_socket: &'a S,
    /// Unicast socket the client receives notifications on; `None` for a
    /// provider-only node.
    pub rx_socket: Option<&'a S>,
    pub timer: &'a Tm,
    pub e2e: &'a R,
    /// SD multicast endpoint the offers are sent to.
    pub sd_multicast: SocketAddrV4,
    /// Re-announce period (the node SD TTL).
    pub period: Duration,
    /// Offered services packed into one combined `OfferService`.
    pub offers: &'a [OfferServiceRequest],
    /// The server's SD state: the session ID and reboot flag of each Offer.
    pub sd_state: &'a SdStateManager,
    pub offer_scratch: &'a mut [u8],
    pub rx_buf: &'a mut [u8],
    pub dispatch: RequestDispatchFn,
    /// Opaque context word forwarded verbatim as `dispatch`'s first arg.
    /// The reusable runtime passes `0` (its trampoline injects the real
    /// ctx); a direct caller passes its own.
    pub dispatch_ctx: usize,
}

/// Drive the full bare-metal SOME/IP integration as ONE future: the
/// caller-supplied server receive future (`recv`, typically
/// `Server::run_with_buffers`) concurrently with the combined-offer
/// announce and the notification RX+dispatch. Spawn this from a single
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
    R: E2EHandle + Clone,
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
        sd_state,
        offer_scratch,
        rx_buf,
        dispatch,
        dispatch_ctx,
    } = cfg;

    let announce = announce_offers_future::<_, _, RUN_OFFER_CAP>(
        sd_socket,
        timer,
        offers,
        sd_multicast,
        sd_state,
        period,
        offer_scratch,
    );

    // RX runs only for a consumer node; for a provider-only node it idles
    // forever so the join arity stays fixed.
    let rx_fut = async move {
        if let Some(rx_socket) = rx_socket {
            event_rx_dispatch_future(rx_socket, e2e, dispatch, dispatch_ctx, rx_buf).await;
        } else {
            core::future::pending::<()>().await;
        }
    };

    // All three loop forever; the join never resolves.
    futures_util::join!(recv, announce, rx_fut);
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
/// buffer (no per-call stack buffer), which `e2e` protects the notification
/// in. Returns the number of subscribers sent to (`>= 0`), or a negative
/// error: `-2` if `scratch` is too small for the header and the payload,
/// protected or not, or if `e2e` cannot protect a payload that long. With no subscribers it returns `0` without protecting
/// the notification, so a payload that only overflows `scratch` once
/// protected is reported once someone subscribes.
///
/// `send` is invoked *after* the subscription handle's critical section is
/// released, not from inside the visitor: this function snapshots the
/// subscriber endpoints into a stack buffer first (no allocation — the
/// buffer is sized to the manager's own per-group cap). `send` therefore
/// need not be reentrancy-safe with respect to the subscription table, and
/// it does not have to be `Send`.
// A SOME/IP notification's full addressing (service/instance/eg/method/
// session) plus payload, scratch, and send sink; a params struct would
// only relocate the list.
#[allow(clippy::too_many_arguments)]
pub fn publish_notification<Sub, R, FSend>(
    subscriptions: &Sub,
    e2e: &R,
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
    R: E2EHandle + ?Sized,
    FSend: FnMut(&[u8], SocketAddrV4),
{
    let Ok(total) = build_notification_datagram(scratch, service_id, method_id, session, payload)
    else {
        return -2;
    };

    // Two-phase: snapshot the subscriber endpoints under the handle's
    // critical section, then transmit after it is released. The visitor
    // therefore captures only a `HeaplessVec` of addresses — it does not
    // capture `send` — which keeps `FSend` free of the `Send` bound that
    // `SubscriptionHandle::for_each_subscriber`'s `&mut dyn FnMut(..) + Send`
    // visitor would otherwise impose on every firmware transmit callback.
    // The buffer's cap is the manager's own per-group cap, so the snapshot
    // can never truncate. Keeping the caller's `send` out of the critical
    // section is also the pattern the trait documents.
    let mut targets: HeaplessVec<SocketAddrV4, { ServerConfig::SUBSCRIBERS_PER_GROUP_CAP }> =
        HeaplessVec::new();
    // Inner scope so the visitor's mutable borrow of `targets` (and the
    // future's borrow of the visitor) both end before the transmit loop
    // reads `targets` back.
    {
        let mut visit = |sub: &Subscriber| {
            let _ = targets.push(sub.address);
        };
        // `for_each_subscriber` is synchronous-backed on the bare-metal
        // handle and resolves to the subscriber count on the first poll;
        // drive it once under a no-op waker.
        let fut =
            subscriptions.for_each_subscriber(service_id, instance_id, event_group_id, &mut visit);
        let mut fut = pin!(fut);
        // SAFETY: NOOP_RAW_WAKER's vtable functions are all no-ops / return
        // the same waker, satisfying the RawWaker contract.
        let waker = unsafe { Waker::from_raw(NOOP_RAW_WAKER) };
        let mut cx = Context::from_waker(&waker);
        let _ = fut.as_mut().poll(&mut cx);
    }

    if targets.is_empty() {
        return 0;
    }
    let Ok(total) = crate::transport::protect_in_place(e2e, scratch, total) else {
        return -2;
    };
    let datagram = &scratch[..total];
    for target in &targets {
        send(datagram, *target);
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    {
        targets.len() as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{ReceivedDatagram, TransportError};
    use core::net::Ipv4Addr;
    use futures_util::FutureExt;

    #[derive(Default)]
    struct RecordingSocket {
        sent: std::sync::Mutex<std::vec::Vec<std::vec::Vec<u8>>>,
    }

    impl TransportSocket for RecordingSocket {
        type SendFuture<'a> = core::future::Ready<Result<(), TransportError>>;
        type RecvFuture<'a> = core::future::Pending<Result<ReceivedDatagram, TransportError>>;

        fn send_to<'a>(&'a self, buf: &'a [u8], _target: SocketAddrV4) -> Self::SendFuture<'a> {
            self.sent.lock().unwrap().push(buf.to_vec());
            core::future::ready(Ok(()))
        }
        fn recv_from<'a>(&'a self, _buf: &'a mut [u8]) -> Self::RecvFuture<'a> {
            core::future::pending()
        }
        fn local_addr(&self) -> Result<SocketAddrV4, TransportError> {
            Ok(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
        }
        fn join_multicast_v4(&self, _: Ipv4Addr, _: Ipv4Addr) -> Result<(), TransportError> {
            Ok(())
        }
        fn leave_multicast_v4(&self, _: Ipv4Addr, _: Ipv4Addr) -> Result<(), TransportError> {
            Ok(())
        }
    }

    struct NeverTimer;

    impl Timer for NeverTimer {
        type SleepFuture<'a> = core::future::Pending<()>;
        fn sleep(&self, _duration: Duration) -> Self::SleepFuture<'_> {
            core::future::pending()
        }
    }

    #[test]
    fn offers_continue_the_server_sd_session_and_reboot_flag() {
        let sd_state = SdStateManager::new();
        let (first_session, _) = sd_state.next_session_id_with_reboot_flag();
        let socket = RecordingSocket::default();
        let offers = [OfferServiceRequest {
            service_id: 0x0042,
            instance_id: 1,
            major_version: 1,
            minor_version: 0,
            ttl: 3,
            local_ip: Ipv4Addr::new(192, 0, 2, 1),
            unicast_port: 30501,
        }];
        let mut scratch = [0u8; 256];

        let announce = announce_offers_future::<_, _, 1>(
            &socket,
            &NeverTimer,
            &offers,
            SocketAddrV4::new(Ipv4Addr::new(239, 255, 0, 255), 30490),
            &sd_state,
            Duration::from_secs(1),
            &mut scratch,
        );
        assert!(announce.now_or_never().is_none());

        let sent = socket.sent.lock().unwrap();
        let datagram = &sent[0];
        let session = u16::from_be_bytes([datagram[10], datagram[11]]);
        assert_eq!(u32::from(session), first_session + 1);
        assert_eq!(
            datagram[16] & 0x80,
            0x80,
            "reboot flag set until the session wraps"
        );
    }
}
