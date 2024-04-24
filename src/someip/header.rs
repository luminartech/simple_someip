use super::{message_type::MessageType, return_code::ReturnCode};

///SOMEIP header (including tp header if present).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Header {
    pub message_id: u32,
    pub length: u32,
    pub request_id: u32,
    pub interface_version: u8,
    pub message_type: MessageType,
    pub return_code: ReturnCode,
}
