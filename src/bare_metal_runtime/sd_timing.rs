//! The runtime's `FindService` timing and its per-node initial delay.

/// SD timing for the runtime's `FindService`. `#[repr(C)]`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SdTiming {
    /// Least delay before the first `FindService`, in milliseconds.
    pub initial_delay_min_ms: u32,
    /// Greatest delay before the first `FindService`, in milliseconds; the
    /// runtime picks one in `initial_delay_min_ms..=initial_delay_max_ms`.
    pub initial_delay_max_ms: u32,
    /// Delay before the first repetition, in milliseconds; each later one
    /// doubles it.
    pub repetitions_base_delay_ms: u32,
    /// Repetitions after the first `FindService`; 0 sends one.
    pub repetitions_max: u8,
}

impl SdTiming {
    /// 10 to 100 ms initial delay, 100 ms base delay, 3 repetitions.
    pub const DEFAULT: Self = Self {
        initial_delay_min_ms: 10,
        initial_delay_max_ms: 100,
        repetitions_base_delay_ms: 100,
        repetitions_max: 3,
    };
}

/// A nonzero seed that differs between nodes on one segment.
pub(super) fn seed(interface: u32, now_ms: u32) -> u32 {
    match interface ^ now_ms.rotate_left(16) {
        0 => 0x9E37_79B9,
        s => s,
    }
}

/// A delay in `timing`'s initial-delay range, picked by a xorshift32 step
/// from `seed`; the minimum when the range is empty.
pub(super) fn draw_initial_delay_ms(timing: &SdTiming, seed: u32) -> u32 {
    let (min, max) = (timing.initial_delay_min_ms, timing.initial_delay_max_ms);
    if max <= min {
        return min;
    }
    let mut x = seed;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    let span = u64::from(max - min) + 1;
    #[allow(clippy::cast_possible_truncation)]
    let offset = (u64::from(x) % span) as u32;
    min + offset
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIMING: SdTiming = SdTiming {
        initial_delay_min_ms: 50,
        initial_delay_max_ms: 150,
        ..SdTiming::DEFAULT
    };

    #[test]
    fn draw_stays_within_the_initial_delay_range() {
        for s in (1..=u32::MAX).step_by(7_919_993) {
            let d = draw_initial_delay_ms(&TIMING, s);
            assert!((50..=150).contains(&d), "seed {s}: {d}");
        }
    }

    #[test]
    fn draw_returns_min_when_max_is_below_min() {
        let timing = SdTiming {
            initial_delay_min_ms: 80,
            initial_delay_max_ms: 20,
            ..SdTiming::DEFAULT
        };
        assert_eq!(draw_initial_delay_ms(&timing, 12345), 80);
    }

    #[test]
    fn draw_handles_the_full_u32_range() {
        let timing = SdTiming {
            initial_delay_min_ms: 0,
            initial_delay_max_ms: u32::MAX,
            ..SdTiming::DEFAULT
        };
        let _ = draw_initial_delay_ms(&timing, 12345);
    }

    #[test]
    fn seed_is_nonzero_and_differs_by_interface() {
        assert_ne!(seed(0, 0), 0);
        assert_ne!(seed(0x7F00_0001, 1000), seed(0x7F00_0002, 1000));
    }
}
