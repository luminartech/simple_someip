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
    /// Creates flags with the given reboot and unicast values.
    #[must_use]
    pub const fn new(reboot: bool, unicast: bool) -> Self {
        Self { reboot, unicast }
    }
    /// Creates SD flags with unicast always set to `true`.
    #[must_use]
    pub const fn new_sd(reboot: bool) -> Self {
        Self {
            reboot,
            unicast: true,
        }
    }
    /// Returns `true` if the reboot flag is set.
    #[must_use]
    pub const fn reboot(self) -> bool {
        self.reboot
    }
    /// Returns `true` if the unicast flag is set.
    #[must_use]
    pub const fn unicast(self) -> bool {
        self.unicast
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_sd_sets_unicast_true() {
        let flags = Flags::new_sd(false);
        assert!(!flags.reboot());
        assert!(flags.unicast());
    }

    #[test]
    fn new_sd_with_reboot_true() {
        let flags = Flags::new_sd(true);
        assert!(flags.reboot());
        assert!(flags.unicast());
    }

    #[test]
    fn unicast_accessor() {
        assert!(Flags::new(false, true).unicast());
        assert!(!Flags::new(false, false).unicast());
    }

    #[test]
    fn roundtrip_both_flags_set() {
        let flags = Flags::new(true, true);
        let byte: u8 = flags.into();
        let back = Flags::from(byte);
        assert_eq!(flags, back);
        assert_eq!(byte, 0b1100_0000);
    }

    #[test]
    fn roundtrip_no_flags_set() {
        let flags = Flags::new(false, false);
        let byte: u8 = flags.into();
        assert_eq!(byte, 0);
        let back = Flags::from(byte);
        assert_eq!(flags, back);
    }
}
