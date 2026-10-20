//! The reusable embassy runtime: owns the executor and the single composed
//! task, builds the receive `Server` from the supplied catalog, registers
//! E2E, and exposes `init`/`poll`/`publish`/`deinit`. A platform provides a
//! [`RuntimeConfig`] (catalog + I/O callbacks + buffer memory) and tick-polls.

use core::cell::RefCell;
use core::mem::MaybeUninit;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicUsize, Ordering};
use core::time::Duration;

use embassy_executor::raw::Executor;
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

use crate::bare_metal_tasks::{SomeipRun, publish_notification, run_someip};
use crate::protocol::sd;
use crate::sd_codec::{
    OfferServiceRequest, SubscribeEventgroupRequest, build_multi_stop_offer_service_datagram,
    next_sd_session,
};
use crate::server::{
    EventPublisher, FindTiming, SdStateManager, Server, ServerConfig, ServerStorage,
    StaticSubscriptionHandle, StaticSubscriptionStorage, SubscriptionManager,
};
use crate::transport::{E2EHandle, NoE2E};

use super::mailbox::RxMailbox;
use super::sd_timing::{SdTiming, draw_initial_delay_ms, seed};
use super::transport::{CallbackFactory, CallbackSocket, CallbackTimer, NowMsFn, Platform, SendFn};

// ── Fixed runtime sizing (catalog-agnostic) ──────────────────────────────
/// RX mailbox slots.
pub const RX_SLOTS: usize = crate::from_env_or(option_env!("SIMPLE_SOMEIP_RX_SLOTS"), 2);
/// Per-slot / per-buffer capacity. One Ethernet-MTU-ish datagram; SOME/IP
/// single datagrams stay under this (TP segmentation not used here).
pub const RX_CAP: usize = 1500;
const MAX_OFFERS: usize = crate::from_env_or(option_env!("SIMPLE_SOMEIP_MAX_OFFERS"), 4);
const MAX_SUBS: usize = crate::from_env_or(option_env!("SIMPLE_SOMEIP_MAX_SUBS"), 1);
const SD_SCRATCH_CAP: usize = 512;
/// Capacity of the API-only send scratch (`RuntimeBuffers::publish_scratch`),
/// shared by `publish` and `deinit`. Sized for modest events rather than a
/// full MTU to keep the always-resident static footprint small (+512 B, not
/// +`RX_CAP`); it caps `publish` payloads at `API_SCRATCH_CAP - 16`. Bump it
/// (with the FW team's RAM sign-off) if a node must emit larger notifications.
/// Must be `>= SD_SCRATCH_CAP` so `deinit`'s combined `StopOffer` datagram fits.
const API_SCRATCH_CAP: usize = 512;
const _: () = assert!(
    API_SCRATCH_CAP >= SD_SCRATCH_CAP,
    "publish_scratch must hold deinit's StopOffer datagram"
);

const DEFAULT_SD_PORT: u16 = 30490;
const DEFAULT_SD_MCAST: u32 = 0xEFFF_00FF; // 239.255.0.255

type Mailbox = RxMailbox<RX_SLOTS, RX_CAP>;
type Sock = CallbackSocket<'static, RX_SLOTS, RX_CAP>;
type Factory = CallbackFactory<'static, RX_SLOTS, RX_CAP>;
type Publisher =
    EventPublisher<&'static dyn E2EHandle, StaticSubscriptionHandle, &'static Sock, Sock>;
type RtServer = Server<
    Factory,
    CallbackTimer,
    &'static dyn E2EHandle,
    StaticSubscriptionHandle,
    &'static Sock,
    &'static SdStateManager,
    &'static Publisher,
>;

/// Platform dispatch sink for inbound messages (decoded by the runtime).
/// Same shape as [`crate::server::NonSdRequestCallback`] — the union
/// contract — so a platform can use one handler for both the server's
/// non-SD observer and the runtime's notification RX path. `ctx` is the
/// opaque word registered at [`init`]; `source` is the sender;
/// `e2e_status` is `0` when not checked, otherwise the code the
/// [`RuntimeConfig::e2e`] handle returned.
pub type DispatchFn = fn(
    ctx: usize,
    source: core::net::SocketAddrV4,
    service_id: u16,
    method_id: u16,
    payload: &[u8],
    e2e_status: u8,
    response_out: &mut [u8],
) -> i32;

/// Bind a platform UDP socket / PCB for `port` and register its receive
/// path (which must call [`on_rx`]). `is_sd` requests the SD multicast
/// group (`mcast`) be joined. Returns 0 on success. The runtime calls this
/// during [`init`] for the SD port + every unique unicast/RX port in the
/// catalog, so the platform doesn't walk the catalog itself.
pub type BindFn = extern "C" fn(port: u16, is_sd: bool, mcast: u32) -> i32;

/// One offered service. Layout matches a platform's generated catalog
/// entry (`#[repr(C)]`); a project's generator emits arrays of these.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct OfferEntry {
    pub service_id: u16,
    pub instance_id: u16,
    pub event_group_id: u16,
    pub unicast_port: u16,
    pub major_version: u8,
    /// Offer TTL in seconds. 0 sends 3 seconds, and a value above
    /// [`MAX_TTL`](crate::protocol::sd::MAX_TTL) is capped to it.
    pub ttl_seconds: u32,
}

/// One subscription (events this node consumes). `#[repr(C)]`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SubEntry {
    pub service_id: u16,
    pub instance_id: u16,
    pub event_group_id: u16,
    pub method_id: u16,
    pub local_rx_port: u16,
    pub major_version: u8,
}

/// Buffer memory the platform owns (so it controls link placement) and
/// hands to the runtime. One `&'static mut` keeps the interface narrow.
pub struct RuntimeBuffers {
    pub unicast: [u8; RX_CAP],
    pub sd: [u8; RX_CAP],
    pub rx: [u8; RX_CAP],
    pub tx_scratch: [u8; RX_CAP],
    pub offer_scratch: [u8; SD_SCRATCH_CAP],
    /// Scratch reserved for the synchronous public API (`publish` /
    /// `deinit`). Kept disjoint from the buffers the spawned `someip_task`
    /// borrows for its entire (never-resolving) lifetime: `publish`/`deinit`
    /// run from the platform's service task and would otherwise take a
    /// second `&mut` to `tx_scratch` / `offer_scratch` while the task still
    /// holds the first — aliasing UB. `publish` and `deinit` never run
    /// concurrently (single serial caller), so they share this one buffer.
    /// Sized at `API_SCRATCH_CAP` (512 B), not `RX_CAP`, to keep the static
    /// footprint small; it caps the max `publish` payload at
    /// `API_SCRATCH_CAP - 16`.
    pub publish_scratch: [u8; API_SCRATCH_CAP],
}

impl Default for RuntimeBuffers {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeBuffers {
    /// All-zero buffers; `const` so it can initialize a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            unicast: [0; RX_CAP],
            sd: [0; RX_CAP],
            rx: [0; RX_CAP],
            tx_scratch: [0; RX_CAP],
            offer_scratch: [0; SD_SCRATCH_CAP],
            publish_scratch: [0; API_SCRATCH_CAP],
        }
    }
}

/// Everything the platform supplies to stand up the runtime.
pub struct RuntimeConfig {
    /// Local interface IPv4 as a host-order `u32`.
    pub interface: u32,
    /// SD port (`0` → 30490) and multicast group (`0` → 239.255.0.255).
    pub sd_port: u16,
    pub sd_mcast: u32,
    pub multicast_loopback: bool,
    /// Platform I/O callbacks.
    pub send: SendFn,
    pub now_ms: NowMsFn,
    pub dispatch: DispatchFn,
    /// Opaque context word passed back verbatim as the first argument of
    /// every `dispatch` invocation (FFI: stash a pointer as `usize`). A
    /// single-instance platform passes `0`.
    pub dispatch_ctx: usize,
    /// Bind PCBs + register the RX path; called per catalog port in `init`.
    pub bind: BindFn,
    /// Generated catalog (borrowed for the duration of `init`).
    pub offers: &'static [OfferEntry],
    pub subscriptions: &'static [SubEntry],
    /// When the runtime sends `FindService` for its subscription.
    pub sd_timing: SdTiming,
    /// RX mailbox the platform's receive path pushes into.
    pub mailbox: &'static Mailbox,
    /// Runtime buffer memory (platform-owned, placement-controlled).
    pub buffers: &'static mut RuntimeBuffers,
    /// Checks the notifications the runtime receives; `None` for no check.
    /// Notifications the runtime publishes are not protected.
    pub e2e: Option<&'static dyn E2EHandle>,
}

// ── Library-owned state ──────────────────────────────────────────────────
static NO_E2E: NoE2E = NoE2E;
static mut E2E: &'static dyn E2EHandle = &NO_E2E;

fn e2e() -> &'static dyn E2EHandle {
    // SAFETY: written once in `init` before any task reads it.
    unsafe { *core::ptr::addr_of!(E2E) }
}
static SUBS_STORAGE: StaticSubscriptionStorage = BlockingMutex::<
    CriticalSectionRawMutex,
    RefCell<SubscriptionManager>,
>::new(RefCell::new(SubscriptionManager::new()));
static SD_STATE: SdStateManager = SdStateManager::new();
static SERVER_STARTED: AtomicBool = AtomicBool::new(false);

static SD_PORT: AtomicU16 = AtomicU16::new(DEFAULT_SD_PORT);
static SD_MCAST: AtomicU32 = AtomicU32::new(DEFAULT_SD_MCAST);
static IFACE: AtomicU32 = AtomicU32::new(0);

static DISPATCH: AtomicUsize = AtomicUsize::new(0); // DispatchFn as usize
static DISPATCH_CTX: AtomicUsize = AtomicUsize::new(0); // opaque ctx word for DISPATCH
static OFFER_SESSION: AtomicU16 = AtomicU16::new(1);
static STOP_SESSION: AtomicU16 = AtomicU16::new(1);
static PUBLISH_SESSION: AtomicU16 = AtomicU16::new(1);

static OFFERS_LEN: AtomicUsize = AtomicUsize::new(0);
static SUBS_LEN: AtomicUsize = AtomicUsize::new(0);
static mut OFFERS: [OfferEntry; MAX_OFFERS] = [OfferEntry {
    service_id: 0,
    instance_id: 0,
    event_group_id: 0,
    unicast_port: 0,
    major_version: 0,
    ttl_seconds: 0,
}; MAX_OFFERS];
static mut SUBS: [SubEntry; MAX_SUBS] = [SubEntry {
    service_id: 0,
    instance_id: 0,
    event_group_id: 0,
    method_id: 0,
    local_rx_port: 0,
    major_version: 0,
}; MAX_SUBS];

static mut UNICAST_SOCK: MaybeUninit<Sock> = MaybeUninit::uninit();
static mut SD_SOCK: MaybeUninit<Sock> = MaybeUninit::uninit();
static mut PUBLISHER: MaybeUninit<Publisher> = MaybeUninit::uninit();
static mut SERVER: MaybeUninit<RtServer> = MaybeUninit::uninit();
static mut BUFS: *mut RuntimeBuffers = core::ptr::null_mut();
static mut MAILBOX: Option<&'static Mailbox> = None;
static SEND_FN: AtomicUsize = AtomicUsize::new(0); // SendFn as usize
static NOW_FN: AtomicUsize = AtomicUsize::new(0); // NowMsFn as usize

static mut EXECUTOR: MaybeUninit<Executor> = MaybeUninit::uninit();
/// Set once the `EXECUTOR` slot is written and the task is spawned — the
/// gate `poll()` checks. Stays false through all of `init`'s fallible work
/// so `poll()` never touches an uninitialized executor.
static EXECUTOR_INIT: AtomicBool = AtomicBool::new(false);
/// Claimed atomically at the top of `init` to make it idempotent and
/// re-entrancy-safe: only the first caller proceeds; a second concurrent or
/// repeat call is rejected (it must NOT adopt the new config or it would
/// silently leak the platform's freshly-handed `&'static mut` buffers).
/// Reset on `init`'s failure paths so a corrected retry can proceed.
static INIT_CLAIMED: AtomicBool = AtomicBool::new(false);
static RUN_READY: AtomicBool = AtomicBool::new(false);

/// embassy's pender — we tick-poll, so wakes are a no-op.
#[unsafe(no_mangle)]
pub extern "Rust" fn __pender(_context: *mut ()) {}

fn offers() -> &'static [OfferEntry] {
    let len = OFFERS_LEN.load(Ordering::Acquire).min(MAX_OFFERS);
    // SAFETY: single writer (init) released OFFERS_LEN before readers run.
    unsafe { core::slice::from_raw_parts(core::ptr::addr_of!(OFFERS).cast::<OfferEntry>(), len) }
}

fn subs() -> &'static [SubEntry] {
    let len = SUBS_LEN.load(Ordering::Acquire).min(MAX_SUBS);
    unsafe { core::slice::from_raw_parts(core::ptr::addr_of!(SUBS).cast::<SubEntry>(), len) }
}

fn find_offer(
    service_id: u16,
    instance_id: u16,
    event_group_id: u16,
) -> Option<&'static OfferEntry> {
    offers().iter().find(|o| {
        o.service_id == service_id
            && o.instance_id == instance_id
            && o.event_group_id == event_group_id
    })
}

/// Node SD TTL in seconds (from the catalog; one node-wide value surfaced
/// per offer). Drives the offer cadence.
fn node_sd_ttl_secs() -> u64 {
    u64::from(offer_ttl(offers().first().map_or(0, |o| o.ttl_seconds)))
}

/// The TTL an offer is sent with: `ttl_seconds`, with 0 meaning 3 seconds and
/// anything above [`sd::MAX_TTL`] capped to it.
fn offer_ttl(ttl_seconds: u32) -> u32 {
    if ttl_seconds == 0 {
        3
    } else {
        ttl_seconds.min(sd::MAX_TTL)
    }
}

fn platform() -> Platform<'static, RX_SLOTS, RX_CAP> {
    // SAFETY: set once in `init` before any reader.
    let mailbox = unsafe { (*core::ptr::addr_of!(MAILBOX)).expect("mailbox set in init") };
    let send: SendFn =
        unsafe { core::mem::transmute::<usize, SendFn>(SEND_FN.load(Ordering::Acquire)) };
    let now: NowMsFn =
        unsafe { core::mem::transmute::<usize, NowMsFn>(NOW_FN.load(Ordering::Acquire)) };
    Platform {
        send,
        now_ms: now,
        mailbox,
        interface: IFACE.load(Ordering::Acquire),
    }
}

/// Forwards a parsed inbound message to the platform dispatch callback.
/// The `_ctx` received from the caller is ignored: the runtime's real
/// ctx lives in [`DISPATCH_CTX`] (registered at [`init`], possibly
/// re-registered later), so loading it here keeps late re-registration
/// coherent — callers register/pass `0`.
fn dispatch(
    _ctx: usize,
    source: core::net::SocketAddrV4,
    service_id: u16,
    method_id: u16,
    payload: &[u8],
    e2e_status: u8,
    response_out: &mut [u8],
) -> i32 {
    let raw = DISPATCH.load(Ordering::Acquire);
    if raw == 0 {
        return -1;
    }
    // SAFETY: stored from a valid DispatchFn in `init`.
    let f: DispatchFn = unsafe { core::mem::transmute::<usize, DispatchFn>(raw) };
    f(
        DISPATCH_CTX.load(Ordering::Acquire),
        source,
        service_id,
        method_id,
        payload,
        e2e_status,
        response_out,
    )
}

fn offer_requests(out: &mut [OfferServiceRequest; MAX_OFFERS]) -> usize {
    let iface = Ipv4Addr::from(IFACE.load(Ordering::Acquire).to_be_bytes());
    let o = offers();
    for (dst, src) in out.iter_mut().zip(o.iter()) {
        *dst = OfferServiceRequest {
            service_id: src.service_id,
            instance_id: src.instance_id,
            major_version: src.major_version,
            minor_version: 0,
            ttl: offer_ttl(src.ttl_seconds),
            local_ip: iface,
            unicast_port: src.unicast_port,
        };
    }
    o.len()
}

const DEFAULT_OFFER_REQUEST: OfferServiceRequest = OfferServiceRequest {
    service_id: 0,
    instance_id: 0,
    major_version: 0,
    minor_version: 0,
    ttl: 0,
    local_ip: Ipv4Addr::UNSPECIFIED,
    unicast_port: 0,
};

/// Build the receive `Server` over the shared callback sockets, accepting
/// subscribes for every offered service and answering Offers of the first
/// subscription's service.
fn build_server(find_timing: FindTiming) -> bool {
    let o = offers();
    let Some(primary) = o.first() else {
        return false;
    };
    let plat = platform();
    let factory = Factory::new(plat);
    let unicast_port = primary.unicast_port;
    let sd_port = SD_PORT.load(Ordering::Acquire);

    // SAFETY: single-init; `init` is the sole caller.
    unsafe {
        (*core::ptr::addr_of_mut!(UNICAST_SOCK)).write(factory.socket(unicast_port));
        (*core::ptr::addr_of_mut!(SD_SOCK)).write(factory.socket(sd_port));
        let unicast_ref: &'static Sock = (*core::ptr::addr_of!(UNICAST_SOCK)).assume_init_ref();
        let e2e = e2e();
        let subs_handle = StaticSubscriptionHandle::new(&SUBS_STORAGE);
        (*core::ptr::addr_of_mut!(PUBLISHER)).write(EventPublisher::new(
            subs_handle,
            unicast_ref,
            e2e,
        ));
    }

    let iface = Ipv4Addr::from(IFACE.load(Ordering::Acquire).to_be_bytes());
    let mut config = ServerConfig::new(primary.service_id, primary.instance_id)
        .with_interface(iface)
        .with_local_port(primary.unicast_port)
        .with_major_version(primary.major_version)
        .with_ttl(Duration::from_secs(u64::from(offer_ttl(
            primary.ttl_seconds,
        ))))
        .with_event_group(primary.event_group_id)
        // The runtime drives a combined multi-offer OfferService announce
        // (all catalog offers in one SD datagram) from its own future, so
        // the server's recv loop must stay silent on SD — otherwise the
        // primary service would be announced twice.
        .with_announce(false)
        .with_find_timing(find_timing);
    for entry in o {
        config = config.with_accepted_offer(
            entry.service_id,
            entry.instance_id,
            entry.major_version,
            entry.event_group_id,
        );
    }
    if let Some(request) = subscribe_request() {
        let Ok(c) = config.try_with_consumed_eventgroup(request) else {
            return false;
        };
        config = c;
    }

    // SAFETY: storages initialized just above / are 'static.
    let storage = unsafe {
        ServerStorage {
            factory,
            timer: CallbackTimer::new(platform().now_ms),
            e2e_registry: e2e(),
            subscriptions: StaticSubscriptionHandle::new(&SUBS_STORAGE),
            unicast_socket: (*core::ptr::addr_of!(UNICAST_SOCK)).assume_init_ref(),
            sd_socket: (*core::ptr::addr_of!(SD_SOCK)).assume_init_ref(),
            sd_state: &SD_STATE,
            publisher: (*core::ptr::addr_of!(PUBLISHER)).assume_init_ref(),
            started: &SERVER_STARTED,
            non_sd_observer: Some((dispatch as crate::server::NonSdRequestCallback, 0)),
        }
    };
    match Server::new_with_handles(storage, config) {
        Ok(s) => {
            unsafe { (*core::ptr::addr_of_mut!(SERVER)).write(s) };
            true
        }
        Err(_) => false,
    }
}

/// The Subscribe the server sends for the first subscription in the catalog.
fn subscribe_request() -> Option<SubscribeEventgroupRequest> {
    subs().first().map(|s| SubscribeEventgroupRequest {
        service_id: s.service_id,
        instance_id: s.instance_id,
        major_version: s.major_version,
        event_group_id: s.event_group_id,
        ttl: sd::MAX_TTL,
        local_ip: Ipv4Addr::from(IFACE.load(Ordering::Acquire).to_be_bytes()),
        local_rx_port: s.local_rx_port,
    })
}

/// The single composed task: receive server (which also answers Offers with
/// Subscribes) + combined-offer announce + notification RX/dispatch.
#[embassy_executor::task]
async fn someip_task() {
    // SAFETY: `init` wrote SERVER + BUFS + MAILBOX before spawning. Each
    // buffer is reborrowed individually through a raw pointer to its own
    // field — deliberately NOT via a whole-struct `&mut *BUFS`. A
    // whole-struct `&mut` would retag every byte of `RuntimeBuffers`
    // (including `publish_scratch`) and stay live for this task's entire
    // never-resolving lifetime, so the synchronous `publish`/`deinit` API
    // taking `&mut (*BUFS).publish_scratch` would alias it (UB). Per-field
    // reborrows retag only their own disjoint byte ranges, leaving
    // `publish_scratch` untouched here and free for the API to borrow.
    let server: &'static RtServer = unsafe { (*core::ptr::addr_of!(SERVER)).assume_init_ref() };
    let unicast: &'static mut [u8; RX_CAP] =
        unsafe { &mut *core::ptr::addr_of_mut!((*BUFS).unicast) };
    let sd: &'static mut [u8; RX_CAP] = unsafe { &mut *core::ptr::addr_of_mut!((*BUFS).sd) };
    let tx_scratch: &'static mut [u8; RX_CAP] =
        unsafe { &mut *core::ptr::addr_of_mut!((*BUFS).tx_scratch) };
    let offer_scratch: &'static mut [u8; SD_SCRATCH_CAP] =
        unsafe { &mut *core::ptr::addr_of_mut!((*BUFS).offer_scratch) };
    let rx: &'static mut [u8; RX_CAP] = unsafe { &mut *core::ptr::addr_of_mut!((*BUFS).rx) };
    // Recv-only: `with_announce(false)` makes `run_with_buffers` skip its
    // announce arm (the runtime announces all offers itself), so the
    // announce_send_buf is never touched — pass an empty slice. Recv,
    // SubscribeAck and the Subscribe answers use unicast/sd/tx_scratch.
    let recv = server.run_with_buffers(unicast, sd, tx_scratch, &mut []);

    let mut reqs = [DEFAULT_OFFER_REQUEST; MAX_OFFERS];
    let n_offers = offer_requests(&mut reqs);

    let plat = platform();
    let sd_socket: &'static Sock = unsafe { (*core::ptr::addr_of!(SD_SOCK)).assume_init_ref() };
    let sd_mcast = SocketAddrV4::new(
        Ipv4Addr::from(SD_MCAST.load(Ordering::Acquire).to_be_bytes()),
        SD_PORT.load(Ordering::Acquire),
    );
    let timer = CallbackTimer::new(plat.now_ms);
    let e2e = e2e();

    // Client RX socket marker (its PCB is pre-bound by the platform).
    let factory = Factory::new(plat);
    let rx_socket = subs().first().map(|s| factory.socket(s.local_rx_port));
    let period = Duration::from_secs(node_sd_ttl_secs());

    run_someip(
        recv,
        SomeipRun {
            sd_socket,
            rx_socket: rx_socket.as_ref(),
            timer: &timer,
            e2e: &e2e,
            sd_multicast: sd_mcast,
            period,
            offers: &reqs[..n_offers],
            offer_session: &OFFER_SESSION,
            offer_scratch,
            rx_buf: rx,
            dispatch,
            // The trampoline injects DISPATCH_CTX itself; pass 0 here.
            dispatch_ctx: 0,
        },
    )
    .await;
}

// ── Public runtime API (the platform's C-FFI forwards to these) ──────────

/// Stand up the runtime from `config`, build the server, and
/// spawn the composed task.
///
/// Returns `0` on success, or a negative error code: `-1` server build
/// failed, `-2` task spawn failed, `-4` the runtime is already initialized
/// (a repeat or re-entrant `init` — the passed `config` and its buffers are
/// NOT adopted). The `-1`/`-2` paths release the init claim so a corrected
/// retry can proceed; `-4` does not (a runtime is already live).
#[allow(clippy::missing_panics_doc)]
// By-value is required, not incidental: `config.buffers` is a `&'static mut`
// reborrowed into `BUFS` (`ptr::from_mut`), which a shared `&RuntimeConfig`
// could not yield. This is also the FFI ownership-transfer boundary — the
// platform hands the runtime its config and buffer memory.
#[allow(clippy::needless_pass_by_value)]
pub fn init(config: RuntimeConfig) -> i32 {
    // Atomically claim init: only the first caller proceeds. Closes the
    // window the old `load`-then-store-at-end guard left open (a re-entrant
    // `init` via a `bind`/`send` callback, or a repeat call) during which a
    // second caller would re-alias `BUFS` and silently leak its buffers.
    if INIT_CLAIMED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return -4;
    }
    IFACE.store(config.interface, Ordering::Release);
    SD_PORT.store(
        if config.sd_port == 0 {
            DEFAULT_SD_PORT
        } else {
            config.sd_port
        },
        Ordering::Release,
    );
    SD_MCAST.store(
        if config.sd_mcast == 0 {
            DEFAULT_SD_MCAST
        } else {
            config.sd_mcast
        },
        Ordering::Release,
    );
    SEND_FN.store(config.send as usize, Ordering::Release);
    NOW_FN.store(config.now_ms as usize, Ordering::Release);
    DISPATCH.store(config.dispatch as usize, Ordering::Release);
    DISPATCH_CTX.store(config.dispatch_ctx, Ordering::Release);
    unsafe {
        *core::ptr::addr_of_mut!(MAILBOX) = Some(config.mailbox);
        *core::ptr::addr_of_mut!(E2E) = config.e2e.unwrap_or(&NO_E2E);
        *core::ptr::addr_of_mut!(BUFS) = core::ptr::from_mut(config.buffers);
    }

    // Copy catalog into static tables.
    let n_offers = config.offers.len().min(MAX_OFFERS);
    for (i, e) in config.offers.iter().take(n_offers).enumerate() {
        unsafe { (*core::ptr::addr_of_mut!(OFFERS))[i] = *e };
    }
    OFFERS_LEN.store(n_offers, Ordering::Release);
    let n_subs = config.subscriptions.len().min(MAX_SUBS);
    for (i, e) in config.subscriptions.iter().take(n_subs).enumerate() {
        unsafe { (*core::ptr::addr_of_mut!(SUBS))[i] = *e };
    }
    SUBS_LEN.store(n_subs, Ordering::Release);

    // Bind PCBs via the platform (registers the RX path → `on_rx`): the SD
    // port, each unique offered unicast port, and each subscription RX port.
    let bind = config.bind;
    bind(
        SD_PORT.load(Ordering::Acquire),
        true,
        SD_MCAST.load(Ordering::Acquire),
    );
    let mut bound: [u16; MAX_OFFERS + MAX_SUBS] = [0; MAX_OFFERS + MAX_SUBS];
    let mut nb = 0usize;
    let mut bind_once = |port: u16| {
        if port != 0 && !bound[..nb].contains(&port) {
            bind(port, false, 0);
            bound[nb] = port;
            nb += 1;
        }
    };
    for o in offers() {
        bind_once(o.unicast_port);
    }
    for s in subs() {
        bind_once(s.local_rx_port);
    }

    let timing = config.sd_timing;
    let find_timing = FindTiming {
        initial_delay: Duration::from_millis(u64::from(draw_initial_delay_ms(
            &timing,
            seed(config.interface, (config.now_ms)()),
        ))),
        repetitions_base_delay: Duration::from_millis(u64::from(timing.repetitions_base_delay_ms)),
        repetitions_max: timing.repetitions_max,
        sd_multicast: SocketAddrV4::new(
            Ipv4Addr::from(SD_MCAST.load(Ordering::Acquire).to_be_bytes()),
            SD_PORT.load(Ordering::Acquire),
        ),
    };
    if !build_server(find_timing) {
        // Release the claim so the platform can retry after fixing config.
        INIT_CLAIMED.store(false, Ordering::Release);
        return -1;
    }

    // SAFETY: single-init guarded by INIT_CLAIMED (claimed above).
    let spawner = unsafe {
        let slot = &mut *core::ptr::addr_of_mut!(EXECUTOR);
        slot.write(Executor::new(core::ptr::null_mut()));
        slot.assume_init_ref().spawner()
    };
    if spawner.spawn(someip_task()).is_err() {
        INIT_CLAIMED.store(false, Ordering::Release);
        return -2;
    }
    // Publish the poll gate LAST: `poll()` must never touch `EXECUTOR`
    // before the slot above is written and the task is spawned.
    EXECUTOR_INIT.store(true, Ordering::Release);
    RUN_READY.store(true, Ordering::Release);
    0
}

/// Tick the executor once. Call every platform main-loop iteration.
pub fn poll() {
    if !EXECUTOR_INIT.load(Ordering::Acquire) {
        return;
    }
    // SAFETY: single-threaded; the platform poll is the sole caller.
    unsafe { (*core::ptr::addr_of!(EXECUTOR)).assume_init_ref().poll() };
}

fn next_publish_session() -> u16 {
    loop {
        let s = PUBLISH_SESSION.fetch_add(1, Ordering::Relaxed);
        if s != 0 {
            return s;
        }
    }
}

/// Publish a notification to all current subscribers. Returns subscriber
/// count, or negative on error (`-1` not ready, `-2` payload too large,
/// `-3` tuple not offered).
///
/// # Safety
/// `payload` must be valid for `len` bytes (or `len == 0`).
pub unsafe fn publish(
    service_id: u16,
    instance_id: u16,
    event_group_id: u16,
    method_id: u16,
    payload: *const u8,
    len: usize,
) -> i32 {
    if !RUN_READY.load(Ordering::Acquire) {
        return -1;
    }
    let Some(offer) = find_offer(service_id, instance_id, event_group_id) else {
        return -3;
    };
    let src_port = offer.unicast_port;
    // The notification is framed into `publish_scratch` (header + payload),
    // so the payload must fit `API_SCRATCH_CAP - SOMEIP_HEADER_LEN`.
    if len > API_SCRATCH_CAP - 16 {
        return -2;
    }
    if len > 0 && payload.is_null() {
        return -1;
    }
    let payload_slice = if len == 0 {
        &[][..]
    } else {
        unsafe { core::slice::from_raw_parts(payload, len) }
    };
    let subs_handle = StaticSubscriptionHandle::new(&SUBS_STORAGE);
    let plat = platform();
    // SAFETY: `publish_scratch` is reserved for the synchronous API and is
    // never borrowed by the spawned `someip_task` (which borrows only its
    // own disjoint fields), so this `&mut` has no aliasing borrower. `publish`
    // and `deinit` are the only users and run serially from the platform's
    // single service task. Reborrowed through `addr_of_mut!` to retag only
    // this field's bytes.
    let scratch = unsafe { &mut *core::ptr::addr_of_mut!((*BUFS).publish_scratch) };
    publish_notification(
        &subs_handle,
        service_id,
        instance_id,
        event_group_id,
        method_id,
        next_publish_session(),
        payload_slice,
        scratch,
        |datagram, dst| {
            let dst_addr = u32::from_be_bytes(dst.ip().octets());
            (plat.send)(
                src_port,
                datagram.as_ptr(),
                datagram.len(),
                dst_addr,
                dst.port(),
            );
        },
    )
}

/// Best-effort `StopOfferService` (one combined datagram) so peers drop us
/// immediately. Idempotent.
pub fn deinit() {
    if !RUN_READY.swap(false, Ordering::AcqRel) {
        return;
    }
    let mut reqs = [DEFAULT_OFFER_REQUEST; MAX_OFFERS];
    let n = offer_requests(&mut reqs);
    if n == 0 {
        return;
    }
    let plat = platform();
    // SAFETY: `publish_scratch` is the API-reserved buffer (see `publish`),
    // never borrowed by `someip_task`; `deinit` is terminal and serial with
    // `publish`. RX_CAP >= SD_SCRATCH_CAP, so it holds the StopOffer datagram.
    // Reborrowed through `addr_of_mut!` to retag only this field's bytes.
    let scratch = unsafe { &mut *core::ptr::addr_of_mut!((*BUFS).publish_scratch) };
    let session = next_sd_session(&STOP_SESSION);
    if let Ok(total) =
        build_multi_stop_offer_service_datagram::<MAX_OFFERS>(scratch, &reqs[..n], session)
    {
        let dst = SD_MCAST.load(Ordering::Acquire);
        let sd_port = SD_PORT.load(Ordering::Acquire);
        (plat.send)(sd_port, scratch.as_ptr(), total, dst, sd_port);
    }
}

/// Push one received datagram into the RX mailbox. The platform's receive
/// path calls this for every inbound UDP datagram.
///
/// # Safety
/// `buf` must be valid for `len` bytes.
pub unsafe fn on_rx(local_port: u16, src_addr: u32, src_port: u16, buf: *const u8, len: usize) {
    if buf.is_null() || len == 0 {
        return;
    }
    // SAFETY: set in init before the platform starts delivering RX.
    if let Some(mailbox) = unsafe { *core::ptr::addr_of!(MAILBOX) } {
        unsafe {
            let _ = mailbox.push(local_port, src_addr, src_port, buf, len);
        }
    }
}
