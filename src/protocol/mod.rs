mod error;
mod header;
mod message;
mod message_id;
mod message_type;
mod return_code;

/// Service Discovery
pub mod sd;

/// SOME/IP-TP
pub mod tp;

pub use error::Error;
pub use header::Header;
pub use message::Message;
pub use message_id::MessageId;
pub use message_type::{MessageType, MessageTypeField};
pub use return_code::ReturnCode;

pub const SD_MESSAGE_ID: MessageId = MessageId::new(crate::SD_MESSAGE_ID_VALUE);
