//! Availability of the service instances a node requires, tracked from the
//! SOME/IP-SD offers and stop offers it receives.

use core::net::SocketAddr;

use crate::protocol::sd::MAX_TTL;
use crate::server::RequiredService;
use crate::{NetEndpoint, OfferedInstance};

/// A change in the availability of a required service instance, seen in the
/// SOME/IP-SD offers and stop offers the node receives.
///
/// The first sender to offer an instance holds it until that sender's stop
/// offer or until the TTL of its last offer runs out. Offers and stop offers
/// from any other sender are ignored while it holds the instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ServiceChange {
    /// The instance is available at `endpoint`: the first offer of the
    /// instance, or an offer from the holding sender that names a different
    /// endpoint. A repeated offer with the same endpoint reports nothing.
    #[non_exhaustive]
    Available {
        /// The required service instance.
        service: RequiredService,
        /// The endpoint the offer names for the instance.
        endpoint: NetEndpoint,
    },
    /// The instance is no longer available: the holding sender sent a stop
    /// offer, or the TTL of its last offer ran out. An offer TTL of
    /// [`MAX_TTL`](crate::protocol::sd::MAX_TTL) never runs out.
    #[non_exhaustive]
    Gone {
        /// The required service instance.
        service: RequiredService,
    },
}

/// The sender holding a required instance and what its last offer said.
#[derive(Debug, Clone, Copy)]
struct Holder {
    sender: SocketAddr,
    endpoint: NetEndpoint,
    /// Time left before the offer's TTL runs out; `None` for
    /// [`MAX_TTL`], which never runs out.
    remaining_ms: Option<u64>,
}

/// The required service an offered `instance` is, whatever its minor version.
fn required(instance: &OfferedInstance) -> RequiredService {
    RequiredService {
        service_id: instance.service_id,
        instance_id: instance.instance_id,
        major_version: instance.major_version,
    }
}

/// A required instance and its holder, if any.
#[derive(Debug, Clone, Copy)]
struct Record {
    service: RequiredService,
    holder: Option<Holder>,
}

impl Record {
    fn available(&self, endpoint: NetEndpoint) -> ServiceChange {
        ServiceChange::Available {
            service: self.service,
            endpoint,
        }
    }

    fn gone(&self) -> ServiceChange {
        ServiceChange::Gone {
            service: self.service,
        }
    }
}

/// The longest wait [`Availability::next_expiry_ms`] returns.
const MAX_WAIT_MS: u32 = i32::MAX.unsigned_abs();

/// Availability of up to `N` required service instances.
///
/// Every method takes the current time of a wrapping millisecond clock and
/// first ages the held records by the time since the previous call. A TTL
/// longer than the clock's range expires on time as long as some method runs
/// at least once every `i32::MAX` ms, which a caller that sleeps for at most
/// [`next_expiry_ms`](Self::next_expiry_ms) does. A record whose TTL has run
/// out stays held until [`expire`](Self::expire) clears it.
pub(crate) struct Availability<const N: usize> {
    records: heapless::Vec<Record, N>,
    /// Only held records age, and a record is held only from an offer on, so
    /// the starting value never ages a record.
    last_ms: u32,
}

impl<const N: usize> Availability<N> {
    /// A table that requires nothing.
    pub(crate) const fn new() -> Self {
        Self {
            records: heapless::Vec::new(),
            last_ms: 0,
        }
    }

    /// Adds a required instance. Returns `true` when it is added or already
    /// present, `false` when the table already holds `N` others.
    pub(crate) fn require(&mut self, service: RequiredService) -> bool {
        self.records.iter().any(|record| record.service == service)
            || self
                .records
                .push(Record {
                    service,
                    holder: None,
                })
                .is_ok()
    }

    /// The required instances, in the order they were added.
    pub(crate) fn services(&self) -> impl Iterator<Item = RequiredService> + '_ {
        self.records.iter().map(|record| record.service)
    }

    /// Records an offer of `instance` from `sender` at `endpoint`, valid for
    /// `ttl` seconds from `now_ms`.
    pub(crate) fn on_offer(
        &mut self,
        now_ms: u32,
        sender: SocketAddr,
        instance: &OfferedInstance,
        endpoint: NetEndpoint,
        ttl: u32,
    ) -> Option<ServiceChange> {
        self.advance(now_ms);
        let record = self.record(instance)?;
        let remaining_ms = (ttl != MAX_TTL).then(|| u64::from(ttl) * 1000);
        match &mut record.holder {
            Some(holder) if holder.sender != sender => return None,
            Some(holder) if holder.endpoint == endpoint => {
                holder.remaining_ms = remaining_ms;
                return None;
            }
            holder => {
                *holder = Some(Holder {
                    sender,
                    endpoint,
                    remaining_ms,
                });
            }
        }
        Some(record.available(endpoint))
    }

    /// Records a stop offer of `instance` from `sender`.
    pub(crate) fn on_stop(
        &mut self,
        now_ms: u32,
        sender: SocketAddr,
        instance: &OfferedInstance,
    ) -> Option<ServiceChange> {
        self.advance(now_ms);
        let record = self.record(instance)?;
        if record.holder?.sender != sender {
            return None;
        }
        record.holder = None;
        Some(record.gone())
    }

    /// Clears every held record whose offer TTL has run out and reports each
    /// as [`ServiceChange::Gone`].
    pub(crate) fn expire(&mut self, now_ms: u32) -> heapless::Vec<ServiceChange, N> {
        self.advance(now_ms);
        let mut changes = heapless::Vec::new();
        for record in &mut self.records {
            if record
                .holder
                .is_some_and(|holder| holder.remaining_ms == Some(0))
            {
                record.holder = None;
                let _ = changes.push(record.gone());
            }
        }
        changes
    }

    /// Milliseconds until the earliest held record's TTL runs out, at most
    /// `i32::MAX`; `None` when no held record can run out.
    pub(crate) fn next_expiry_ms(&mut self, now_ms: u32) -> Option<u32> {
        self.advance(now_ms);
        self.records
            .iter()
            .filter_map(|record| record.holder?.remaining_ms)
            .min()
            .map(|remaining| u32::try_from(remaining).map_or(MAX_WAIT_MS, |ms| ms.min(MAX_WAIT_MS)))
    }

    fn record(&mut self, instance: &OfferedInstance) -> Option<&mut Record> {
        let service = required(instance);
        self.records
            .iter_mut()
            .find(|record| record.service == service)
    }

    fn advance(&mut self, now_ms: u32) {
        let elapsed = u64::from(now_ms.wrapping_sub(self.last_ms));
        self.last_ms = now_ms;
        for holder in self
            .records
            .iter_mut()
            .filter_map(|record| record.holder.as_mut())
        {
            if let Some(remaining) = &mut holder.remaining_ms {
                *remaining = remaining.saturating_sub(elapsed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::sd::MAX_TTL;

    const SVC: u16 = 0x1234;
    const OTHER_SVC: u16 = 0x5678;
    const INST: u16 = 1;
    const MAJOR: u8 = 1;
    const CAP: u32 = i32::MAX as u32;

    fn instance_of(service_id: u16, instance_id: u16, major_version: u8) -> OfferedInstance {
        OfferedInstance {
            service_id,
            instance_id,
            major_version,
            minor_version: 0,
        }
    }

    fn instance() -> OfferedInstance {
        instance_of(SVC, INST, MAJOR)
    }

    fn sender(host: u8) -> SocketAddr {
        SocketAddr::from(([192, 168, 0, host], 30490))
    }

    fn endpoint(port: u16) -> NetEndpoint {
        NetEndpoint::udp(SocketAddr::from(([192, 168, 0, 10], port)))
    }

    fn service_of(service_id: u16) -> RequiredService {
        RequiredService {
            service_id,
            instance_id: INST,
            major_version: MAJOR,
        }
    }

    fn available(endpoint: NetEndpoint) -> ServiceChange {
        ServiceChange::Available {
            service: service_of(SVC),
            endpoint,
        }
    }

    fn gone() -> ServiceChange {
        ServiceChange::Gone {
            service: service_of(SVC),
        }
    }

    fn table() -> Availability<4> {
        let mut table = Availability::new();
        assert!(table.require(service_of(SVC)));
        table
    }

    #[test]
    fn first_offer_is_available() {
        let mut table = table();
        assert_eq!(
            table.on_offer(0, sender(1), &instance(), endpoint(30501), 3),
            Some(available(endpoint(30501)))
        );
    }

    #[test]
    fn a_repeated_offer_reports_nothing() {
        let mut table = table();
        table.on_offer(0, sender(1), &instance(), endpoint(30501), 3);
        assert_eq!(
            table.on_offer(1000, sender(1), &instance(), endpoint(30501), 3),
            None
        );
    }

    #[test]
    fn same_sender_new_endpoint_is_available_again() {
        let mut table = table();
        table.on_offer(0, sender(1), &instance(), endpoint(30501), 3);
        assert_eq!(
            table.on_offer(1000, sender(1), &instance(), endpoint(30502), 3),
            Some(available(endpoint(30502)))
        );
        assert_eq!(
            table.on_offer(2000, sender(1), &instance(), endpoint(30502), 3),
            None
        );
    }

    #[test]
    fn another_sender_is_ignored_while_one_holds_the_record() {
        let mut table = table();
        table.on_offer(0, sender(1), &instance(), endpoint(30501), 3);
        assert_eq!(
            table.on_offer(1000, sender(2), &instance(), endpoint(30502), 3),
            None
        );
        // The holder's endpoint is unchanged, so its own repeat reports nothing.
        assert_eq!(
            table.on_offer(2000, sender(1), &instance(), endpoint(30501), 3),
            None
        );
    }

    #[test]
    fn a_stop_from_another_sender_is_ignored() {
        let mut table = table();
        table.on_offer(0, sender(1), &instance(), endpoint(30501), 3);
        assert_eq!(table.on_stop(1000, sender(2), &instance()), None);
        assert_eq!(table.on_stop(2000, sender(1), &instance()), Some(gone()));
    }

    #[test]
    fn after_the_holders_stop_any_sender_is_available() {
        let mut table = table();
        table.on_offer(0, sender(1), &instance(), endpoint(30501), 3);
        assert_eq!(table.on_stop(1000, sender(1), &instance()), Some(gone()));
        assert_eq!(
            table.on_offer(2000, sender(2), &instance(), endpoint(30502), 3),
            Some(available(endpoint(30502)))
        );
    }

    #[test]
    fn an_unrequired_instance_or_major_version_is_ignored() {
        let mut table = table();
        let unrequired = [
            instance_of(OTHER_SVC, INST, MAJOR),
            instance_of(SVC, INST + 1, MAJOR),
            instance_of(SVC, INST, MAJOR + 1),
        ];
        for other in &unrequired {
            assert_eq!(
                table.on_offer(0, sender(1), other, endpoint(30501), 3),
                None
            );
        }
        // None of them took the record.
        assert_eq!(
            table.on_offer(0, sender(2), &instance(), endpoint(30502), 3),
            Some(available(endpoint(30502)))
        );
        for other in &unrequired {
            assert_eq!(table.on_stop(0, sender(2), other), None);
        }
    }

    #[test]
    fn a_stop_for_an_unrecorded_service_reports_nothing() {
        let mut table = table();
        assert_eq!(table.on_stop(0, sender(1), &instance()), None);
        assert_eq!(table.on_stop(0, sender(1), &instance()), None);
        assert_eq!(
            table.on_stop(0, sender(1), &instance_of(OTHER_SVC, INST, MAJOR)),
            None
        );
        assert_eq!(
            table.on_stop(0, sender(1), &instance_of(OTHER_SVC, INST, MAJOR)),
            None
        );
    }

    #[test]
    fn expires_at_its_ttl() {
        let mut table = table();
        table.on_offer(1000, sender(1), &instance(), endpoint(30501), 3);
        assert_eq!(table.next_expiry_ms(1500), Some(2500));
        assert!(table.expire(3999).is_empty());
        assert_eq!(table.expire(4000).as_slice(), [gone()]);
        assert!(table.expire(4001).is_empty());
        assert_eq!(table.next_expiry_ms(4001), None);
        assert_eq!(
            table.on_offer(5000, sender(2), &instance(), endpoint(30502), 3),
            Some(available(endpoint(30502)))
        );
    }

    #[test]
    fn a_renewal_pushes_the_deadline_out() {
        let mut table = table();
        table.on_offer(1000, sender(1), &instance(), endpoint(30501), 3);
        assert_eq!(
            table.on_offer(3000, sender(1), &instance(), endpoint(30501), 3),
            None
        );
        assert!(table.expire(4000).is_empty());
        assert!(table.expire(5999).is_empty());
        assert_eq!(table.expire(6000).as_slice(), [gone()]);
    }

    #[test]
    fn max_ttl_never_expires() {
        let mut table = table();
        table.on_offer(0, sender(1), &instance(), endpoint(30501), MAX_TTL);
        assert_eq!(table.next_expiry_ms(0), None);
        let mut now = 0u32;
        // Well past MAX_TTL seconds, in steps the clock can represent.
        for _ in 0..16 {
            now = now.wrapping_add(CAP);
            assert!(table.expire(now).is_empty());
            assert_eq!(table.next_expiry_ms(now), None);
        }
    }

    #[test]
    fn expiry_survives_the_clock_wrap() {
        let mut table = table();
        let start = u32::MAX - 999;
        table.on_offer(start, sender(1), &instance(), endpoint(30501), 3);
        assert!(table.expire(start.wrapping_add(999)).is_empty());
        assert!(table.expire(start.wrapping_add(1000)).is_empty());
        assert!(table.expire(start.wrapping_add(2999)).is_empty());
        assert_eq!(table.expire(start.wrapping_add(3000)).as_slice(), [gone()]);
    }

    #[test]
    fn a_ttl_longer_than_the_clock_range_expires_on_time() {
        const TTL_S: u32 = 60 * 24 * 60 * 60;
        let deadline = u64::from(TTL_S) * 1000;
        assert!(deadline > u64::from(u32::MAX));
        let mut table = table();
        table.on_offer(0, sender(1), &instance(), endpoint(30501), TTL_S);
        let mut now = 0u32;
        let mut elapsed = 0u64;
        while deadline - elapsed > u64::from(CAP) {
            assert_eq!(table.next_expiry_ms(now), Some(CAP));
            now = now.wrapping_add(CAP);
            elapsed += u64::from(CAP);
            assert!(table.expire(now).is_empty());
        }
        let rest = u32::try_from(deadline - elapsed).unwrap();
        assert_eq!(table.next_expiry_ms(now), Some(rest));
        assert!(table.expire(now.wrapping_add(rest - 1)).is_empty());
        assert_eq!(table.expire(now.wrapping_add(rest)).as_slice(), [gone()]);
    }

    #[test]
    fn next_expiry_is_capped() {
        let mut table = table();
        assert_eq!(table.next_expiry_ms(0), None);
        table.on_offer(0, sender(1), &instance(), endpoint(30501), MAX_TTL - 1);
        assert_eq!(table.next_expiry_ms(0), Some(CAP));
    }

    #[test]
    fn the_time_before_the_first_offer_does_not_age_it() {
        let mut table = table();
        let late = 3_000_000_000;
        table.on_offer(late, sender(1), &instance(), endpoint(30501), 3);
        assert_eq!(table.next_expiry_ms(late), Some(3000));
        assert!(table.expire(late + 2999).is_empty());
        assert_eq!(table.expire(late + 3000).as_slice(), [gone()]);
    }

    #[test]
    fn services_lists_each_required_instance_once_in_order() {
        let mut table: Availability<4> = Availability::new();
        assert!(table.require(service_of(OTHER_SVC)));
        assert!(table.require(service_of(SVC)));
        assert!(table.require(service_of(OTHER_SVC)));
        assert!(
            table
                .services()
                .eq([service_of(OTHER_SVC), service_of(SVC)])
        );
    }

    #[test]
    fn require_deduplicates_and_reports_full() {
        let mut table: Availability<1> = Availability::new();
        assert!(table.require(service_of(SVC)));
        assert!(!table.require(service_of(OTHER_SVC)));
        assert!(table.require(service_of(SVC)));
    }
}
