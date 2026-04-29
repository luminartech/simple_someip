//! Phase-20-pre flash-size measurement probe.
//!
//! Mirrors halo PR #4429's `rust_simple_someip` C-callable FFI
//! surface (header encode/decode + E2E protect/check round-trips)
//! to get a realistic post-link flash-size floor on
//! `thumbv7em-none-eabihf` for what a Halo TC4D `rust_simple_someip`
//! staticlib would cost.
//!
//! NOT production code. Exposes `#[no_mangle] extern "C"` entry
//! points only so post-link DCE keeps what an actual FFI consumer
//! would reach, and discards everything else.

#![no_std]

use core::alloc::{GlobalAlloc, Layout};
use core::panic::PanicInfo;
use core::ptr;
use core::slice;

/// Stub allocator. Some transitive dep pulls `extern crate alloc`
/// even with simple-someip's `default-features = false`, requiring a
/// `#[global_allocator]` link target. The codec-only FFI surface
/// (header encode + E2E protect/check) never actually allocates, so
/// this stub returning null on alloc is sound for the probe; if any
/// path it fronts ever does allocate, that's an explicit FFI-design
/// bug surfaced at link time, not silent corruption at runtime.
struct PanicAllocator;

unsafe impl GlobalAlloc for PanicAllocator {
    unsafe fn alloc(&self, _: Layout) -> *mut u8 {
        ptr::null_mut()
    }
    unsafe fn dealloc(&self, _: *mut u8, _: Layout) {}
}

#[global_allocator]
static ALLOC: PanicAllocator = PanicAllocator;

use simple_someip::WireFormat;
use simple_someip::e2e::{
    Profile4Config, Profile4State, Profile5Config, Profile5State, check_profile4, check_profile5,
    protect_profile4, protect_profile5,
};
use simple_someip::protocol::{Header, MessageId, MessageType, MessageTypeField, ReturnCode};

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
    let Ok(msg_type_raw) = MessageType::try_from(h.message_type & 0xBF) else {
        return 0;
    };
    let msg_type = MessageTypeField::new(msg_type_raw, (h.message_type & 0x20) != 0);
    let Ok(ret_code) = ReturnCode::try_from(h.return_code) else {
        return 0;
    };
    let header = Header::new(
        message_id,
        request_id,
        h.protocol_version,
        h.interface_version,
        msg_type,
        ret_code,
        0,
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
    if buf.len() < payload_len + 12 {
        return out;
    }
    let Ok(protected_len) =
        protect_profile4(&config, &mut protect_state, payload, &mut buf)
    else {
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
    let mut protect_state =
        Profile5State::with_initial_counter((initial_counter & 0xFF) as u8);

    let mut buf = [0u8; 1500];
    if buf.len() < payload_len + 4 {
        return out;
    }
    let Ok(protected_len) =
        protect_profile5(&config, &mut protect_state, payload, &mut buf)
    else {
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
