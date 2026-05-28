//! UDP wire helpers: encode/decode SOME/IP messages with E2E
//! protect/check applied inline.
//!
//! Lifted from `socket_manager::spawn_socket_loop` so the encode/decode
//! pipeline does not depend on the tokio-bound `SocketManager`. The same
//! helpers will be called by the runtime-agnostic client/server once they
//! own UDP sockets directly.

use std::sync::{Arc, Mutex};
use std::vec;
use std::vec::Vec;

use crate::e2e::{E2ECheckStatus, E2EKey, E2ERegistry, PROFILE4_HEADER_SIZE};
use crate::protocol::{Message, MessageView};
use crate::traits::{PayloadWireFormat, WireFormat};

use super::error::Error;

/// Result of decoding a single UDP datagram into a SOME/IP message.
///
/// `e2e_status` is `Some` when the message's `MessageId` had an active E2E
/// configuration in the registry, `None` otherwise.
#[derive(Debug)]
pub(crate) struct Decoded<P> {
    pub message: Message<P>,
    pub e2e_status: Option<E2ECheckStatus>,
}

/// Parse `bytes` as a SOME/IP message, applying any registered E2E check.
///
/// On success returns the decoded message together with the E2E check
/// outcome. The check status is `None` when no E2E config exists for the
/// message's `MessageId`.
///
/// # Errors
/// Returns the underlying parse / payload decode error wrapped in
/// [`Error`].
pub(crate) fn decode_with_e2e<P>(
    bytes: &[u8],
    e2e_registry: &Arc<Mutex<E2ERegistry>>,
) -> Result<Decoded<P>, Error>
where
    P: PayloadWireFormat,
{
    let view = MessageView::parse(bytes)?;
    let header = view.header().to_owned();
    let upper_header = header.upper_header_bytes();
    let key = E2EKey::from_message_id(header.message_id());
    let payload_bytes = view.payload_bytes();

    let (e2e_status, effective_payload) = {
        let mut registry = e2e_registry
            .lock()
            .expect("e2e registry lock poisoned");
        match registry.check(key, payload_bytes, upper_header) {
            Some((status, stripped)) => (Some(status), stripped),
            None => (None, payload_bytes),
        }
    };

    let payload = P::from_payload_bytes(header.message_id(), effective_payload)?;
    Ok(Decoded {
        message: Message::new(header, payload),
        e2e_status,
    })
}

/// Encode `message` into `buf`, applying any registered E2E protection.
///
/// Returns the number of bytes written. `buf` is grown if E2E expansion
/// makes the protected frame larger than the original encoded size.
///
/// # Errors
/// Returns the underlying encode error wrapped in [`Error`]. E2E protect
/// errors are logged but treated as non-fatal (the original unprotected
/// message is sent).
pub(crate) fn encode_with_e2e<P>(
    message: &Message<P>,
    buf: &mut Vec<u8>,
    e2e_registry: &Arc<Mutex<E2ERegistry>>,
) -> Result<usize, Error>
where
    P: PayloadWireFormat,
{
    let mut message_length = message.encode(&mut buf.as_mut_slice())?;

    let key = E2EKey::from_message_id(message.header().message_id());
    let mut registry = e2e_registry
        .lock()
        .expect("e2e registry lock poisoned");
    if registry.contains_key(&key) {
        let original_payload = buf[16..message_length].to_vec();
        let upper_header: [u8; 8] = buf[8..16].try_into().expect("upper header slice");
        let mut protected = vec![0u8; original_payload.len() + PROFILE4_HEADER_SIZE];
        match registry.protect(key, &original_payload, upper_header, &mut protected) {
            Some(Ok(protected_len)) => {
                #[allow(clippy::cast_possible_truncation)]
                let new_length: u32 = 8 + protected_len as u32;
                buf[4..8].copy_from_slice(&new_length.to_be_bytes());
                if 16 + protected_len > buf.len() {
                    buf.resize(16 + protected_len, 0);
                }
                buf[16..16 + protected_len].copy_from_slice(&protected[..protected_len]);
                message_length = 16 + protected_len;
            }
            Some(Err(e)) => {
                tracing::error!("E2E protect error: {:?}", e);
            }
            None => unreachable!("contains_key was true"),
        }
    }

    Ok(message_length)
}
