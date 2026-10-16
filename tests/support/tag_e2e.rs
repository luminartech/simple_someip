//! A test [`E2EHandle`]: for one message ID, `protect` prefixes the payload
//! with [`TAG`], and `check` strips it ([`PASSED`]) or reports it missing
//! ([`TAG_MISSING`]). Every other message ID passes through untouched.

use core::net::IpAddr;
use core::num::NonZeroU8;

use simple_someip::protocol::MessageId;
use simple_someip::{E2EHandle, E2EProtectError};

pub const TAG: u8 = 0xE2;
pub const PASSED: NonZeroU8 = NonZeroU8::new(1).unwrap();
pub const TAG_MISSING: NonZeroU8 = NonZeroU8::new(2).unwrap();

#[derive(Clone, Copy)]
pub struct TagE2E {
    message_id: MessageId,
}

impl TagE2E {
    pub fn new(message_id: MessageId) -> Self {
        Self { message_id }
    }
}

impl E2EHandle for TagE2E {
    fn check<'a>(
        &self,
        _source: IpAddr,
        message_id: MessageId,
        payload: &'a [u8],
        _upper_header: [u8; 8],
    ) -> Option<(NonZeroU8, &'a [u8])> {
        if message_id != self.message_id {
            return None;
        }
        Some(match payload.split_first() {
            Some((&TAG, rest)) => (PASSED, rest),
            _ => (TAG_MISSING, payload),
        })
    }

    fn protect(
        &self,
        message_id: MessageId,
        payload: &[u8],
        _upper_header: [u8; 8],
        out: &mut [u8],
    ) -> Option<Result<usize, E2EProtectError>> {
        if message_id != self.message_id {
            return None;
        }
        let needed = payload.len() + 1;
        if out.len() < needed {
            return Some(Err(E2EProtectError::BufferTooSmall {
                needed,
                available: out.len(),
            }));
        }
        out[0] = TAG;
        out[1..needed].copy_from_slice(payload);
        Some(Ok(needed))
    }

    fn reset_source(&self, _source: IpAddr) {}
}
