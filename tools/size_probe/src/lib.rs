//! no_std measurement probes for `thumbv7em-none-eabihf`. Two live here:
//!
//! 1. **Flash-size probe** (phase 20-pre): mirrors halo PR #4429's
//!    `rust_simple_someip` C-callable FFI surface (header
//!    encode/decode + E2E protect/check round-trips) to get a
//!    realistic post-link flash-size floor for what a Halo TC4D
//!    `rust_simple_someip` staticlib would cost.
//! 2. **Client-future layout probe** (PR 0, issue #125): instantiates
//!    the client run future with zero-behavior deps so
//!    `-Zprint-type-sizes` reports its real on-target layout — see
//!    `client_future_probe` below and `tools/capture_type_sizes.sh`.
//!
//! NOT production code. Exposes `#[no_mangle] extern "C"` entry
//! points only so post-link DCE keeps what an actual FFI consumer
//! would reach, and discards everything else. Flash measurements that
//! predate the layout probe only linked the codec symbols — see the
//! comparability caveat in this crate's `Cargo.toml`.

#![no_std]

use core::alloc::{GlobalAlloc, Layout};
use core::panic::PanicInfo;
use core::ptr;
use core::slice;

/// Stub allocator that returns null on every `alloc` call. Some
/// transitive dep pulls `extern crate alloc` even with simple-someip's
/// `default-features = false`, requiring a `#[global_allocator]`
/// link target. The codec-only FFI surface (header encode + E2E
/// protect/check) never actually allocates, and the client layout
/// probe rides the `client,bare_metal` combo certified alloc-free by
/// the TC4 audit (CI's `nm` alloc-symbol gate), so a `null_mut()`
/// return is sound for the probe — if any code path ever does try to alloc,
/// the resulting null deref shows up at runtime as the FFI-design
/// bug it is, rather than being papered over with hidden heap usage.
/// (Named `NullAllocator` rather than `PanicAllocator` because it
/// returns null, it doesn't panic, and the original name was
/// confusing reviewers into thinking link-time failures were the
/// failure mode.)
struct NullAllocator;

unsafe impl GlobalAlloc for NullAllocator {
    unsafe fn alloc(&self, _: Layout) -> *mut u8 {
        ptr::null_mut()
    }
    unsafe fn dealloc(&self, _: *mut u8, _: Layout) {}
}

#[global_allocator]
static ALLOC: NullAllocator = NullAllocator;

use simple_someip::WireFormat;
use simple_someip::e2e::{
    Profile4Config, Profile4State, Profile5Config, Profile5State, check_profile4, check_profile5,
    protect_profile4, protect_profile5,
};
use simple_someip::protocol::{Header, MessageId, MessageTypeField, ReturnCode};

/// Required for no_std staticlib targeting thumbv7em.
#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    loop {}
}

// ── SOME/IP header encode ───────────────────────────────────────────

#[repr(C)]
pub struct CSomeIpHeader {
    pub service_id: u16,
    pub method_id: u16,
    pub length: u32,
    pub client_id: u16,
    pub session_id: u16,
    pub protocol_version: u8,
    pub interface_version: u8,
    pub message_type: u8,
    pub return_code: u8,
}

/// # Safety
/// Caller must ensure `header` points to a valid `CSomeIpHeader` and
/// `buf` points to at least `buf_len` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn someip_header_encode(
    header: *const CSomeIpHeader,
    buf: *mut u8,
    buf_len: usize,
) -> usize {
    if header.is_null() || buf.is_null() || buf_len < 16 {
        return 0;
    }
    let h = unsafe { &*header };
    let message_id = MessageId::new_from_service_and_method(h.service_id, h.method_id);
    let request_id = (u32::from(h.client_id) << 16) | u32::from(h.session_id);
    // Validate the message_type byte BEFORE splitting off the TP
    // flag. `MessageTypeField::try_from` rejects any reserved-bit
    // pattern (e.g. `0x40`) instead of silently masking it down to
    // `Request` like a `MessageType::try_from(byte & 0xBF)` would.
    let Ok(msg_type) = MessageTypeField::try_from(h.message_type) else {
        return 0;
    };
    let Ok(ret_code) = ReturnCode::try_from(h.return_code) else {
        return 0;
    };
    // SOME/IP `length` covers (request_id .. end-of-payload) — the 8
    // SOME/IP header bytes after the length field plus the payload.
    // `Header::new` takes `payload_len` and adds 8 internally, so
    // recover payload_len from the caller's full-`length`.
    let payload_len = match (h.length as usize).checked_sub(8) {
        Some(p) => p,
        None => return 0,
    };
    let header = Header::new(
        message_id,
        request_id,
        h.protocol_version,
        h.interface_version,
        msg_type,
        ret_code,
        payload_len,
    );
    let out = unsafe { slice::from_raw_parts_mut(buf, buf_len) };
    header.encode(&mut &mut out[..]).unwrap_or(0)
}

// ── E2E Profile 4 protect + check ───────────────────────────────────

#[repr(C)]
pub struct E2eRoundTripResult {
    pub ok: i32,
    pub protected_len: u32,
    pub check_status: u8,
    pub counter: u32,
    pub payload_match: i32,
}

/// # Safety
/// Caller must ensure `payload` points to at least `payload_len`
/// readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2e_profile4_round_trip(
    payload: *const u8,
    payload_len: usize,
    initial_counter: u16,
) -> E2eRoundTripResult {
    let mut out = E2eRoundTripResult {
        ok: 0,
        protected_len: 0,
        check_status: 0,
        counter: 0,
        payload_match: 0,
    };
    if payload.is_null() {
        return out;
    }
    let payload = unsafe { slice::from_raw_parts(payload, payload_len) };

    let config = Profile4Config::new(0x1234_5678, 15);
    let mut protect_state = Profile4State::with_initial_counter(initial_counter);

    // Probe-only stack buffer; production code uses caller-supplied storage.
    let mut buf = [0u8; 1500];
    let Some(needed) = payload_len.checked_add(12) else {
        return out;
    };
    if buf.len() < needed {
        return out;
    }
    let Ok(protected_len) = protect_profile4(&config, &mut protect_state, payload, &mut buf) else {
        return out;
    };

    let mut check_state = Profile4State::with_initial_counter(initial_counter);
    let result = check_profile4(&config, &mut check_state, &buf[..protected_len]);

    out.ok = 1;
    out.protected_len = protected_len as u32;
    out.check_status = result.status as u8;
    out.counter = result.counter.unwrap_or(0);
    out.payload_match = i32::from(result.payload == Some(payload));
    out
}

// ── E2E Profile 5 protect + check ───────────────────────────────────

/// # Safety
/// Caller must ensure `payload` points to at least `payload_len`
/// readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn e2e_profile5_round_trip(
    payload: *const u8,
    payload_len: usize,
    initial_counter: u16,
) -> E2eRoundTripResult {
    let mut out = E2eRoundTripResult {
        ok: 0,
        protected_len: 0,
        check_status: 0,
        counter: 0,
        payload_match: 0,
    };
    if payload.is_null() {
        return out;
    }
    let payload = unsafe { slice::from_raw_parts(payload, payload_len) };

    let Ok(payload_len_u16) = u16::try_from(payload_len) else {
        return out;
    };
    let config = Profile5Config::new(0x1234, payload_len_u16, 15);
    let mut protect_state = Profile5State::with_initial_counter((initial_counter & 0xFF) as u8);

    let mut buf = [0u8; 1500];
    let Some(needed) = payload_len.checked_add(4) else {
        return out;
    };
    if buf.len() < needed {
        return out;
    }
    let Ok(protected_len) = protect_profile5(&config, &mut protect_state, payload, &mut buf) else {
        return out;
    };

    let mut check_state = Profile5State::with_initial_counter((initial_counter & 0xFF) as u8);
    let result = check_profile5(&config, &mut check_state, &buf[..protected_len]);

    out.ok = 1;
    out.protected_len = protected_len as u32;
    out.check_status = result.status as u8;
    out.counter = result.counter.unwrap_or(0);
    out.payload_match = i32::from(result.payload == Some(payload));
    out
}

// ── Client-future layout probe (PR 0, issue #125) ────────────────────
//
// Instantiates the client's run-future and (transitively) the
// per-socket loop with zero-behavior deps so `-Zprint-type-sizes`
// reports their REAL thumbv7em layouts during this crate's codegen.
// The entry point is `extern "C"` + `#[unsafe(no_mangle)]` purely so
// post-link DCE keeps the instantiation; nothing ever calls it on
// hardware. The server probe is deferred to PR 3 (needs the no-alloc
// `new_with_handles` static plumbing that PR 3 builds anyway).

mod client_future_probe {
    use simple_someip::client::Error as ClientError;
    use simple_someip::client::{ClientUpdate, ControlMessage, ReceivedMessage, SendMessage};
    use simple_someip::protocol::sd::RebootFlag;
    use simple_someip::protocol::{MessageId, sd};
    use simple_someip::transport::probe::{
        NullE2ERegistry, NullFactory, NullInterface, NullSpawner, NullTimer,
    };
    use simple_someip::{Client, ClientDeps, PayloadWireFormat, WireFormat};

    // `RawPayload` is std-gated (heap `Vec` SD storage), so the probe
    // carries its own minimal no_std `PayloadWireFormat` impl —
    // heapless 4-entry SD storage, mirroring the crate-internal
    // `protocol::sd::test_support::TestPayload` (which is
    // `pub(crate)` and unreachable from here). A real firmware build
    // ships its own payload type the same way.

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct ProbeSdHeader {
        flags: sd::Flags,
        entries: heapless::Vec<sd::Entry, 4>,
        options: heapless::Vec<sd::Options, 4>,
    }

    impl WireFormat for ProbeSdHeader {
        fn required_size(&self) -> usize {
            sd::Header::new(self.flags, &self.entries, &self.options).required_size()
        }
        fn encode<T: embedded_io::Write>(
            &self,
            writer: &mut T,
        ) -> Result<usize, simple_someip::protocol::Error> {
            sd::Header::new(self.flags, &self.entries, &self.options).encode(writer)
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct ProbePayload {
        header: ProbeSdHeader,
    }

    impl PayloadWireFormat for ProbePayload {
        type SdHeader = ProbeSdHeader;
        fn message_id(&self) -> MessageId {
            MessageId::SD
        }
        fn as_sd_header(&self) -> Option<&ProbeSdHeader> {
            Some(&self.header)
        }
        fn from_payload_bytes(
            message_id: MessageId,
            payload: &[u8],
        ) -> Result<Self, simple_someip::protocol::Error> {
            match message_id {
                MessageId::SD => {
                    let view = sd::SdHeaderView::parse(payload)?;
                    let mut entries = heapless::Vec::new();
                    for ev in view.entries() {
                        entries.push(ev.to_owned().unwrap()).ok();
                    }
                    let mut options = heapless::Vec::new();
                    for ov in view.options() {
                        options.push(ov.to_owned().unwrap()).ok();
                    }
                    Ok(Self {
                        header: ProbeSdHeader {
                            flags: view.flags(),
                            entries,
                            options,
                        },
                    })
                }
                _ => Err(simple_someip::protocol::Error::UnsupportedMessageID(
                    message_id,
                )),
            }
        }
        fn new_sd_payload(header: &ProbeSdHeader) -> Self {
            Self {
                header: header.clone(),
            }
        }
        fn sd_flags(&self) -> Option<sd::Flags> {
            Some(self.header.flags)
        }
        fn required_size(&self) -> usize {
            self.header.required_size()
        }
        fn encode<T: embedded_io::Write>(
            &self,
            writer: &mut T,
        ) -> Result<usize, simple_someip::protocol::Error> {
            self.header.encode(writer)
        }
        fn new_subscription_sd_header(
            service_id: u16,
            instance_id: u16,
            major_version: u8,
            ttl: u32,
            event_group_id: u16,
            client_ip: core::net::Ipv4Addr,
            protocol: sd::TransportProtocol,
            client_port: u16,
            reboot_flag: sd::RebootFlag,
        ) -> ProbeSdHeader {
            let entry = sd::Entry::SubscribeEventGroup(sd::EventGroupEntry::new(
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
            let mut entries = heapless::Vec::new();
            entries.push(entry).unwrap();
            let mut options = heapless::Vec::new();
            options.push(endpoint).unwrap();
            ProbeSdHeader {
                flags: sd::Flags::new_sd(reboot_flag),
                entries,
                options,
            }
        }
        fn set_reboot_flag(header: &mut ProbeSdHeader, reboot: sd::RebootFlag) {
            header.flags = sd::Flags::new(bool::from(reboot), header.flags.unicast());
        }
    }

    // Entry list mirrors `tests/bare_metal_e2e.rs`'s `E2ETestChannels`
    // (with `ProbePayload` standing in for the std-gated `RawPayload`)
    // so the probed futures see the same channel shapes as the host
    // capture.
    simple_someip::define_static_channels! {
        name: ProbeChannels,
        oneshot: [
            (Result<(), ClientError>, 16),
            (Result<ProbePayload, ClientError>, 8),
            (Result<RebootFlag, ClientError>, 8),
        ],
        bounded: [
            ((ControlMessage<ProbePayload, ProbeChannels>, 4), 4),
            ((SendMessage<ProbePayload, ProbeChannels>, 16), 8),
            ((Result<ReceivedMessage<ProbePayload>, ClientError>, 16), 8),
        ],
        unbounded: [
            (ClientUpdate<ProbePayload>, 4),
        ],
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn probe_client_run_future_size() -> usize {
        let deps = ClientDeps {
            factory: NullFactory,
            spawner: NullSpawner,
            timer: NullTimer,
            e2e_registry: NullE2ERegistry,
            interface: NullInterface(core::net::Ipv4Addr::LOCALHOST),
        };
        let (_client, _updates, run_fut) = Client::<
            ProbePayload,
            NullE2ERegistry,
            NullInterface,
            ProbeChannels,
        >::new_with_deps(deps, false);
        core::mem::size_of_val(&run_fut)
    }
}
