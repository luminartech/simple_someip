/// Newtype for the flags byte in the SD protocol.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Flags(u8);

impl From<u8> for Flags {
    /// Only the two least significant bits are used.
    fn from(value: u8) -> Self {
        Self(value & 0b0000_0011)
    }
}

impl From<Flags> for u8 {
    fn from(flags: Flags) -> u8 {
        flags.0
    }
}

impl Flags {
    pub fn reboot_flag(&self) -> bool {
        self.0 & 0b0000_0001 != 0
    }
    pub fn set_reboot_flag(&mut self, value: bool) {
        if value {
            self.0 |= 0b0000_0001;
        } else {
            self.0 &= 0b1111_1110;
        }
    }
    pub fn unicast_flag(&self) -> bool {
        self.0 & 0b0000_0010 != 0
    }
    pub fn set_unicast_flag(&mut self, value: bool) {
        if value {
            self.0 |= 0b0000_0010;
        } else {
            self.0 &= 0b1111_1101;
        }
    }
}
