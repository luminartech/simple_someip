const REBOOT_FLAG: u8 = 0b1000_0000;
const UNICAST_FLAG: u8 = 0b0100_0000;

/// Flags byte in the SD protocol.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Flags {
    reboot: bool,
    unicast: bool,
}

impl From<u8> for Flags {
    /// Only the two most significant bits are used.
    fn from(value: u8) -> Self {
        Self {
            reboot: value & REBOOT_FLAG != 0,
            unicast: value & UNICAST_FLAG != 0,
        }
    }
}

impl From<Flags> for u8 {
    fn from(flags: Flags) -> u8 {
        let mut value = 0;
        if flags.reboot {
            value |= REBOOT_FLAG;
        }
        if flags.unicast {
            value |= UNICAST_FLAG;
        }
        value
    }
}

impl Flags {
    pub fn new(reboot: bool, unicast: bool) -> Self {
        Self { reboot, unicast }
    }
    pub fn new_sd(reboot: bool) -> Self {
        Self {
            reboot,
            unicast: true,
        }
    }
}
