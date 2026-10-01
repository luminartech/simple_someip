//! `SomeipUnderTest` over simple-someip's bare-metal runtime, with std UDP
//! sockets standing in for the platform's network stack.
//!
//! The runtime is a process-wide singleton that starts once per process, so
//! each scenario needs a process of its own; cargo nextest provides that.
//!
//! The runtime reports only inbound requests and notifications to the
//! platform, so this adapter produces `Event` and `Request` observations and
//! never `ServiceAvailable`, `ServiceGone`, `Subscribed` or `RuntimeError`.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use simple_someip::bare_metal_runtime as rt;

use crate::interop::consts::*;
use crate::interop::runtime::*;

/// How long the poller sleeps between executor ticks.
const TICK: Duration = Duration::from_millis(1);

const ALREADY_STARTED: &str = "the bare-metal runtime was already started in this process; \
     run the interop tests with cargo nextest";

static MAILBOX: rt::RxMailbox<{ rt::RX_SLOTS }, { rt::RX_CAP }> = rt::RxMailbox::new();
static mut BUFFERS: rt::RuntimeBuffers = rt::RuntimeBuffers::new();
/// Claimed before `BUFFERS` is borrowed, so at most one `&mut` to it exists.
static STARTED: AtomicBool = AtomicBool::new(false);
/// The host sockets, by local port.
static SOCKETS: Mutex<Vec<(u16, UdpSocket)>> = Mutex::new(Vec::new());
/// Ports the host transport could not bind (the runtime ignores `bind`'s
/// result, so `start` checks this instead).
static BIND_ERRORS: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Serializes `rt::on_rx`: the mailbox takes one producer at a time, and
/// every bound socket has its own reader thread.
static RX: Mutex<()> = Mutex::new(());
/// Where `dispatch` reports observations.
static OBS: Mutex<Option<Sender<Observation>>> = Mutex::new(None);
static EPOCH: OnceLock<Instant> = OnceLock::new();

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Requests from the test thread to the poller, the runtime's one caller
/// after `init`.
enum Cmd {
    Publish {
        event: u16,
        payload: Vec<u8>,
        reply: Sender<i32>,
    },
    Deinit,
}

pub struct BareMetalRt {
    obs: Receiver<Observation>,
    cmds: Sender<Cmd>,
    poller: Option<JoinHandle<()>>,
}

impl SomeipUnderTest for BareMetalRt {
    const NAME: &'static str = "bare-metal runtime";

    fn start(setup: Setup) -> Self {
        let Some(offer) = setup.offer else {
            unsupported(
                Self::NAME,
                "starting without an offer (init builds its server from the first offer)",
            )
        };
        let offers = vec![rt::OfferEntry {
            service_id: SVC,
            instance_id: INST,
            event_group_id: EG,
            unicast_port: SERVER_PORT,
            major_version: MAJOR,
            ttl_seconds: offer.ttl_s,
        }];
        let subscriptions: Vec<_> = setup
            .consume
            .filter(|c| c.subscribe)
            .map(|c| subscription(c.e2e.as_ref()))
            .into_iter()
            .collect();

        assert!(!STARTED.swap(true, Ordering::AcqRel), "{ALREADY_STARTED}");
        let (tx, obs) = mpsc::channel();
        *lock(&OBS) = Some(tx);
        let rc = rt::init(rt::RuntimeConfig {
            interface: u32::from(OUR_IP),
            sd_port: SD_PORT,
            sd_mcast: u32::from(SD_GROUP),
            multicast_loopback: true,
            send,
            now_ms,
            dispatch,
            dispatch_ctx: 0,
            bind,
            offers: offers.leak(),
            subscriptions: subscriptions.leak(),
            mailbox: &MAILBOX,
            // SAFETY: `STARTED` was claimed above and is released only when
            // `init` fails before spawning the task that borrows these
            // buffers, so this is the one live reference to `BUFFERS`.
            buffers: unsafe { &mut *std::ptr::addr_of_mut!(BUFFERS) },
        });
        match rc {
            0 => {}
            -4 => panic!("{ALREADY_STARTED}"),
            rc => {
                STARTED.store(false, Ordering::Release);
                panic!("{}: init failed with {rc}", Self::NAME);
            }
        }
        let errors = lock(&BIND_ERRORS);
        assert!(errors.is_empty(), "{}: {}", Self::NAME, errors.join("; "));
        drop(errors);

        let (cmds, rx) = mpsc::channel();
        let poller = std::thread::Builder::new()
            .name("bare-metal-poller".into())
            .spawn(move || run_poller(&rx))
            .expect("could not spawn the poller thread");
        Self {
            obs,
            cmds,
            poller: Some(poller),
        }
    }

    /// `deinit` is the runtime's only way to stop offering: it sends a
    /// StopOffer, after which the runtime is no longer polled.
    fn stop_offer(&mut self) {
        let _ = self.cmds.send(Cmd::Deinit);
    }

    fn publish(&mut self, event: u16, payload: &[u8]) {
        let (reply, rc) = mpsc::channel();
        let cmd = Cmd::Publish {
            event,
            payload: payload.to_vec(),
            reply,
        };
        let rc = self.cmds.send(cmd).ok().and_then(|()| rc.recv().ok());
        match rc {
            Some(n) if n >= 0 => {}
            Some(rc) => panic!("{}: publishing 0x{event:04X} returned {rc}", Self::NAME),
            None => panic!("{}: publishing 0x{event:04X} after stop_offer", Self::NAME),
        }
    }

    /// The runtime has no field support (no initial value on subscribe), so
    /// a field update is published like an event.
    fn set_field(&mut self, field: u16, payload: &[u8]) {
        self.publish(field, payload);
    }

    fn unsubscribe(&mut self) {
        unsupported(Self::NAME, "unsubscribing")
    }

    fn call(&mut self, _method: u16, _payload: &[u8], _timeout: Duration) -> CallOutcome {
        unsupported(Self::NAME, "client-side method calls")
    }

    fn fire_and_forget(&mut self, _method: u16, _payload: &[u8]) {
        unsupported(Self::NAME, "client-side method calls")
    }

    fn next(&mut self, timeout: Duration) -> Option<Observation> {
        self.obs.recv_timeout(timeout).ok()
    }
}

impl Drop for BareMetalRt {
    fn drop(&mut self) {
        let _ = self.cmds.send(Cmd::Deinit);
        if let Some(poller) = self.poller.take() {
            let _ = poller.join();
        }
        *lock(&OBS) = None;
    }
}

/// The one `SubEntry` for `EG`, E2E-protected if `e2e` is given.
fn subscription(e2e: Option<&E2eSpec>) -> rt::SubEntry {
    let (data_id, data_length, max_delta) = e2e.map_or((0, 0, 0), |e2e| {
        assert!(
            e2e.data_length_bits % 8 == 0,
            "E2E data length {} bits is not a whole number of bytes",
            e2e.data_length_bits
        );
        (e2e.data_id, e2e.data_length_bits / 8, e2e.max_delta)
    });
    rt::SubEntry {
        service_id: SVC,
        instance_id: INST,
        event_group_id: EG,
        method_id: EVENT,
        local_rx_port: CLIENT_PORT,
        major_version: MAJOR,
        e2e_enabled: e2e.is_some(),
        e2e_data_id: data_id,
        // Bytes of protected data, excluding the E2E header.
        e2e_data_length: data_length,
        e2e_max_delta: u16::from(max_delta),
    }
}

/// Ticks the runtime and runs the test thread's requests between ticks, so
/// `poll`, `publish` and `deinit` share this one serial caller.
fn run_poller(cmds: &Receiver<Cmd>) {
    loop {
        loop {
            match cmds.try_recv() {
                Ok(Cmd::Publish {
                    event,
                    payload,
                    reply,
                }) => {
                    // SAFETY: `payload` is valid for `payload.len()` bytes for
                    // the duration of the call.
                    let rc = unsafe {
                        rt::publish(SVC, INST, EG, event, payload.as_ptr(), payload.len())
                    };
                    let _ = reply.send(rc);
                }
                Ok(Cmd::Deinit) | Err(TryRecvError::Disconnected) => {
                    rt::deinit();
                    return;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        rt::poll();
        std::thread::sleep(TICK);
    }
}

/// Binds the host socket for `port` and starts its reader thread. The
/// runtime cannot restart in-process, so sockets and readers live until the
/// process exits.
extern "C" fn bind(port: u16, is_sd: bool, mcast: u32) -> i32 {
    match bind_socket(port, is_sd, Ipv4Addr::from(mcast)) {
        Ok(sock) => {
            lock(&SOCKETS).push((port, sock));
            0
        }
        Err(e) => {
            lock(&BIND_ERRORS).push(format!("could not bind port {port}: {e}"));
            -1
        }
    }
}

fn bind_socket(port: u16, is_sd: bool, mcast: Ipv4Addr) -> io::Result<UdpSocket> {
    let s = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, None)?;
    s.set_reuse_address(true)?;
    s.set_reuse_port(true)?;
    let addr = if is_sd { Ipv4Addr::UNSPECIFIED } else { OUR_IP };
    s.bind(&SocketAddrV4::new(addr, port).into())?;
    if is_sd {
        s.join_multicast_v4(&mcast, &OUR_IP)?;
        s.set_multicast_if_v4(&OUR_IP)?;
        s.set_multicast_loop_v4(true)?;
    }
    let sock = UdpSocket::from(s);
    let reader = sock.try_clone()?;
    std::thread::Builder::new()
        .name(format!("bare-metal-rx-{port}"))
        .spawn(move || read_into_mailbox(&reader, port))?;
    Ok(sock)
}

fn read_into_mailbox(sock: &UdpSocket, port: u16) {
    let mut buf = [0u8; rt::RX_CAP];
    while let Ok((n, src)) = sock.recv_from(&mut buf) {
        let SocketAddr::V4(src) = src else { continue };
        // With multicast loopback on, the SD socket also hears our own sends.
        if *src.ip() == OUR_IP && src.port() == port {
            continue;
        }
        let _rx = lock(&RX);
        // SAFETY: `buf` is valid for `n` bytes, and holding `RX` keeps this
        // the mailbox's only producer.
        unsafe { rt::on_rx(port, u32::from(*src.ip()), src.port(), buf.as_ptr(), n) };
    }
}

extern "C" fn send(local_port: u16, buf: *const u8, len: usize, dst: u32, dst_port: u16) -> i32 {
    // SAFETY: the runtime passes a buffer valid for `len` bytes for the
    // duration of the call.
    let bytes = unsafe { std::slice::from_raw_parts(buf, len) };
    let sockets = lock(&SOCKETS);
    let Some((_, sock)) = sockets.iter().find(|(p, _)| *p == local_port) else {
        eprintln!("bare-metal runtime: send from unbound port {local_port}");
        return -1;
    };
    let to = SocketAddrV4::new(Ipv4Addr::from(dst), dst_port);
    sock.send_to(bytes, to).map_or(-1, |_| 0)
}

extern "C" fn now_ms() -> u32 {
    // The runtime's clock wraps at `u32::MAX`, so truncation is intended.
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u32
}

/// The runtime's dispatch sink. Its notification path passes no response
/// buffer; its server-request path passes one, which this fills by echoing
/// the request payload.
fn dispatch(
    _ctx: usize,
    _source: SocketAddrV4,
    service: u16,
    method: u16,
    payload: &[u8],
    e2e_status: u8,
    response_out: &mut [u8],
) -> i32 {
    let (observation, rc) = if response_out.is_empty() {
        let event = Observation::Event {
            service,
            event: method,
            payload: payload.to_vec(),
            e2e_ok: (e2e_status != 0).then_some(e2e_status == 1),
        };
        (event, -1)
    } else {
        let n = payload.len().min(response_out.len());
        response_out[..n].copy_from_slice(&payload[..n]);
        let request = Observation::Request {
            service,
            method,
            no_return: None,
            session: None,
            payload: payload.to_vec(),
        };
        (request, i32::try_from(n).unwrap_or(-1))
    };
    if let Some(tx) = lock(&OBS).as_ref() {
        let _ = tx.send(observation);
    }
    rc
}
