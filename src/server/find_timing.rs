//! When the run future sends `FindService`, and the per-node initial delay.

use core::time::Duration;

/// When the run future sends `FindService` for the
/// [`ServerConfig::consumed_eventgroups`](super::ServerConfig::consumed_eventgroups)
/// and [`ServerConfig::required_services`](super::ServerConfig::required_services)
/// not found: once after an initial delay in
/// `initial_delay_min..=initial_delay_max`, then after
/// `repetitions_base_delay` doubled each time, `repetitions_max` more times.
/// No `FindService` is sent after that until an instance that was found is
/// gone, which starts the schedule again. Start from [`FindTiming::DEFAULT`]
/// and set the fields to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct FindTiming {
    /// Least delay before the first `FindService`.
    pub initial_delay_min: Duration,
    /// Greatest delay before the first `FindService`. The server picks the
    /// delay in whole milliseconds from `initial_delay_min..=initial_delay_max`,
    /// seeded by its interface address and port, the endpoints it subscribes
    /// from and the services it requires, and uses `initial_delay_min` when
    /// this is below it.
    pub initial_delay_max: Duration,
    /// Delay before the first repetition; each later one doubles it.
    pub repetitions_base_delay: Duration,
    /// Number of repetitions after the first `FindService`; 0 sends one.
    pub repetitions_max: u8,
}

impl FindTiming {
    /// 10 to 100 ms initial delay, 100 ms base delay, 3 repetitions.
    pub const DEFAULT: Self = Self {
        initial_delay_min: Duration::from_millis(10),
        initial_delay_max: Duration::from_millis(100),
        repetitions_base_delay: Duration::from_millis(100),
        repetitions_max: 3,
    };

    /// A delay in the initial-delay range, picked by a xorshift32 step from
    /// `seed`; the minimum when the range is empty.
    pub(super) fn initial_delay(&self, seed: u32) -> Duration {
        let Some(span) = self.initial_delay_max.checked_sub(self.initial_delay_min) else {
            return self.initial_delay_min;
        };
        let span_ms = u64::try_from(span.as_millis()).unwrap_or(u64::MAX);
        let mut x = seed;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        let offset = u64::from(x) % span_ms.saturating_add(1);
        self.initial_delay_min
            .saturating_add(Duration::from_millis(offset))
    }
}

/// A nonzero seed from `config`'s interface address and port, the endpoints
/// of its consumed eventgroups and its required services, which differ
/// between nodes on one segment.
pub(super) fn seed(config: &super::ServerConfig) -> u32 {
    let endpoints = core::iter::once((config.interface, config.local_port())).chain(
        config
            .consumed_eventgroups
            .iter()
            .map(|request| (request.local_ip, request.local_rx_port)),
    );
    let words = endpoints
        .map(|(ip, port)| u32::from(ip) ^ u32::from(port).rotate_left(16))
        .chain(config.required_services.iter().map(|service| {
            u32::from(service.service_id()) << 16
                ^ u32::from(service.instance_id())
                ^ u32::from(service.major_version()).rotate_left(8)
        }));
    let mixed = words.fold(0u32, |hash, word| {
        (hash.rotate_left(5) ^ word).wrapping_mul(0x9E37_79B1)
    });
    match mixed {
        0 => 0x9E37_79B9,
        s => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServiceInstance;
    use crate::server::Offering;
    use core::net::Ipv4Addr;

    const TIMING: FindTiming = FindTiming {
        initial_delay_min: Duration::from_millis(50),
        initial_delay_max: Duration::from_millis(150),
        ..FindTiming::DEFAULT
    };

    #[test]
    fn the_initial_delay_stays_within_its_range() {
        for s in (1..=u32::MAX).step_by(7_919_993) {
            let d = TIMING.initial_delay(s);
            assert!(
                (TIMING.initial_delay_min..=TIMING.initial_delay_max).contains(&d),
                "seed {s}: {d:?}"
            );
        }
    }

    #[test]
    fn the_initial_delay_is_the_minimum_when_the_maximum_is_below_it() {
        let timing = FindTiming {
            initial_delay_min: Duration::from_millis(80),
            initial_delay_max: Duration::from_millis(20),
            ..FindTiming::DEFAULT
        };
        assert_eq!(timing.initial_delay(12345), Duration::from_millis(80));
    }

    #[test]
    fn the_initial_delay_handles_the_full_duration_range() {
        let timing = FindTiming {
            initial_delay_min: Duration::ZERO,
            initial_delay_max: Duration::MAX,
            ..FindTiming::DEFAULT
        };
        let _ = timing.initial_delay(12345);
    }

    fn consumer(interface: Ipv4Addr, local_ip: Ipv4Addr) -> super::super::ServerConfig {
        super::super::ServerConfig::new(
            Offering::new(ServiceInstance::new(0x1234, 1, 1).unwrap()).with_local_port(30501),
        )
        .with_interface(interface)
        .with_consumed_eventgroup(crate::sd_codec::SubscribeEventgroupRequest {
            service: ServiceInstance::new(0x5678, 1, 1).unwrap(),
            event_group_id: 1,
            ttl: 3,
            local_ip,
            local_rx_port: 40100,
        })
    }

    #[test]
    fn the_seed_is_nonzero_and_differs_by_interface() {
        assert_ne!(
            seed(&super::super::ServerConfig::new(Offering::new(
                ServiceInstance::new(0, 0, 1).unwrap()
            ))),
            0
        );
        assert_ne!(
            seed(&consumer(Ipv4Addr::LOCALHOST, Ipv4Addr::LOCALHOST)),
            seed(&consumer(Ipv4Addr::new(127, 0, 0, 2), Ipv4Addr::LOCALHOST))
        );
    }

    #[test]
    fn the_seed_differs_by_required_service_when_the_interface_is_unspecified() {
        let requiring = |service_id| {
            super::super::ServerConfig::consumer_only()
                .with_required_service(ServiceInstance::new(service_id, 1, 1).unwrap())
        };

        assert_ne!(seed(&requiring(0x6001)), seed(&requiring(0x6002)));
    }

    #[test]
    fn the_seed_differs_by_subscribing_endpoint_when_the_interface_is_unspecified() {
        assert_ne!(
            seed(&consumer(
                Ipv4Addr::UNSPECIFIED,
                Ipv4Addr::new(192, 0, 2, 1)
            )),
            seed(&consumer(
                Ipv4Addr::UNSPECIFIED,
                Ipv4Addr::new(192, 0, 2, 2)
            ))
        );
    }
}
