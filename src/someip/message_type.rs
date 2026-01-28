///Message types of a SOME/IP message.
#[derive(Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Variant {
    Request = 0x0,
    RequestNoReturn = 0x1,
    Notification = 0x2,
    Response = 0x80,
    Error = 0x81,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageType {
    pub is_tp: bool,
    pub variant: Variant,
}
