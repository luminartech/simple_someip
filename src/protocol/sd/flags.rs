const REBOOT_FLAG: u8 = 0b1000_0000;
const UNICAST_FLAG: u8 = 0b0100_0000;

/// Whether the sender has recently rebooted, as encoded in the SOME/IP-SD flags byte.
///
/// Per AUTOSAR SOME/IP-SD, this flag is set to [`RebootFlag::RecentlyRebooted`] from
/// startup until the session counter wraps from `0xFFFF` to `1`, then permanently
/// cleared to [`RebootFlag::Continuous`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RebootFlag {
    /// Sender has recently rebooted; session counter has not yet wrapped.
    RecentlyRebooted,
    /// Sender is running continuously; session counter has wrapped at least once.
    Continuous,
}

impl From<bool> for RebootFlag {
    fn from(b: bool) -> Self {
        if b {
            Self::RecentlyRebooted
        } else {
            Self::Continuous
        }
    }
}

impl From<RebootFlag> for bool {
    fn from(r: RebootFlag) -> Self {
        matches!(r, RebootFlag::RecentlyRebooted)
    }
}

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
    pub fn new_sd(reboot: RebootFlag) -> Self {
        Self {
            reboot: bool::from(reboot),
            unicast: true,
        }
    }
    /// Returns the reboot flag.
    #[must_use]
    pub fn reboot(self) -> RebootFlag {
        RebootFlag::from(self.reboot)
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
        let flags = Flags::new_sd(RebootFlag::Continuous);
        assert_eq!(flags.reboot(), RebootFlag::Continuous);
        assert!(flags.unicast());
    }

    #[test]
    fn new_sd_with_reboot_true() {
        let flags = Flags::new_sd(RebootFlag::RecentlyRebooted);
        assert_eq!(flags.reboot(), RebootFlag::RecentlyRebooted);
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
