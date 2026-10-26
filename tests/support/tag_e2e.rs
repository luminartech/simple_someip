//! A test [`E2EHandle`]: for one message ID (or every message ID), `protect`
//! prefixes the payload with [`TAG`], and `check` strips it ([`PASSED`]) or
//! reports it missing ([`TAG_MISSING`]). Every other message ID passes
//! through untouched.

use core::net::IpAddr;
use core::num::NonZeroU8;

use simple_someip::protocol::MessageId;
use simple_someip::{E2EHandle, E2EProtectError};

pub const TAG: u8 = 0xE2;
pub const PASSED: NonZeroU8 = NonZeroU8::new(1).unwrap();
pub const TAG_MISSING: NonZeroU8 = NonZeroU8::new(2).unwrap();

#[derive(Clone, Copy)]
pub struct TagE2E {
    message_id: Option<MessageId>,
}

impl TagE2E {
    pub fn new(message_id: MessageId) -> Self {
        Self {
            message_id: Some(message_id),
        }
    }

    /// Tags every message ID.
    #[allow(dead_code)]
    pub fn every_message() -> Self {
        Self { message_id: None }
    }

    fn applies_to(&self, message_id: MessageId) -> bool {
        self.message_id.is_none_or(|id| id == message_id)
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
        if !self.applies_to(message_id) {
            return None;
        }
        Some(match payload.split_first() {
            Some((&TAG, rest)) => (PASSED, rest),
            _ => (TAG_MISSING, payload),
        })
    }

    fn protect<'b>(
        &self,
        message_id: MessageId,
        buf: &'b mut [u8],
        payload_len: usize,
        _upper_header: [u8; 8],
    ) -> Option<Result<&'b [u8], E2EProtectError>> {
        if !self.applies_to(message_id) {
            return None;
        }
        let needed = payload_len + 1;
        if buf.len() < needed {
            return Some(Err(E2EProtectError::BufferTooSmall {
                needed,
                available: buf.len(),
            }));
        }
        buf.copy_within(..payload_len, 1);
        buf[0] = TAG;
        Some(Ok(&buf[..needed]))
    }

    fn reset_source(&self, _source: IpAddr) {}
}
