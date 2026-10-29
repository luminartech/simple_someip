//! The service instances a server finds: which of them are offered, by
//! whom and until when, tracked from the SOME/IP-SD messages it receives.

use core::net::{SocketAddr, SocketAddrV4};
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::Poll;

use futures_util::task::AtomicWaker;

use super::ServerConfig;
use crate::protocol::sd::MAX_TTL;
use crate::sd_codec::FindTarget;
use crate::{NetEndpoint, ServiceInstance};

/// A change in the availability of a service instance the server finds: one
/// of [`ServerConfig::consumed_eventgroups`] or
/// [`ServerConfig::required_services`].
///
/// An instance is available once an `OfferService` names it with an IPv4
/// endpoint. The first sender to offer it holds it until that sender's
/// `StopOfferService`, or until the TTL of its last offer runs out: between
/// TTL and TTL + 1 s after the server handled that offer, later while its
/// receive loop is busy. While it holds the instance,
/// only its offers are answered with a `SubscribeEventgroup`, and offers and
/// stop offers from any other sender are ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ServiceChange {
    /// The instance is available at `endpoint`: the first offer of the
    /// instance, or an offer from the holding sender that names a different
    /// endpoint. A repeated offer with the same endpoint reports nothing.
    #[non_exhaustive]
    Available {
        /// The service instance.
        service: ServiceInstance,
        /// The endpoint the offer names for the instance.
        endpoint: NetEndpoint,
    },
    /// The instance is no longer available: the holding sender sent a stop
    /// offer, or the TTL of its last offer ran out. An offer TTL of
    /// [`MAX_TTL`] never runs out.
    #[non_exhaustive]
    Gone {
        /// The service instance.
        service: ServiceInstance,
    },
}

/// Callback invoked for each [`ServiceChange`], in the order the server sees
/// them, from the server's receive loop: the loop receives nothing until it
/// returns. `ctx` is the opaque word registered with the callback, passed
/// back verbatim.
pub type ServiceObserver = fn(ctx: usize, change: ServiceChange);

/// Most service instances a server finds: one per consumed eventgroup and
/// required service.
pub(super) const FIND_TARGETS_CAP: usize =
    ServerConfig::CONSUMED_EVENTGROUPS_CAP + ServerConfig::REQUIRED_SERVICES_CAP;

/// `FindService` TTL, in seconds, for a required service that is not
/// consumed.
const REQUIRED_FIND_TTL: u32 = 3;

/// How often [`Discovery::tick`] runs while an offer can run out.
pub(super) const TICK: core::time::Duration = core::time::Duration::from_secs(1);

/// A service instance the server finds.
#[derive(Debug, Clone, Copy)]
struct Target {
    find: FindTarget,
    /// Whether a consumed eventgroup names the instance, so it is found only
    /// once subscribed to.
    consumed: bool,
}

/// The instances a server finds and which of them are found. The receive
/// loop's [`Discovery`] writes the found flags; the find loop reads them.
pub(super) struct Discovered {
    targets: heapless::Vec<Target, FIND_TARGETS_CAP>,
    found: [AtomicBool; FIND_TARGETS_CAP],
    lost: AtomicBool,
    waker: AtomicWaker,
}

impl Discovered {
    /// The instances of `config`'s consumed eventgroups, then those of its
    /// required services, each once. A consumed instance is found with the
    /// TTL of its first eventgroup.
    pub(super) fn new(config: &ServerConfig) -> Self {
        let consumed = config.consumed_eventgroups.iter().map(|request| Target {
            find: FindTarget {
                service: request.service,
                ttl: request.ttl,
            },
            consumed: true,
        });
        let required = config.required_services.iter().map(|&service| Target {
            find: FindTarget {
                service,
                ttl: REQUIRED_FIND_TTL,
            },
            consumed: false,
        });
        let mut targets = heapless::Vec::new();
        for target in consumed.chain(required) {
            let known = targets
                .iter()
                .any(|known: &Target| known.find.service == target.find.service);
            if !known {
                let _ = targets.push(target);
            }
        }
        Self {
            targets,
            found: [const { AtomicBool::new(false) }; FIND_TARGETS_CAP],
            lost: AtomicBool::new(false),
            waker: AtomicWaker::new(),
        }
    }

    /// Whether the server finds no instance.
    pub(super) fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    /// The `FindService` entries for the instances not found, in order.
    pub(super) fn unfound(&self) -> impl Iterator<Item = FindTarget> + Clone + '_ {
        self.targets
            .iter()
            .zip(&self.found)
            .filter(|(_, found)| !found.load(Ordering::Relaxed))
            .map(|(target, _)| target.find)
    }

    /// Forgets the instances lost so far; [`Self::lost`] then waits for the
    /// next one.
    pub(super) fn forget_losses(&self) {
        self.lost.store(false, Ordering::Relaxed);
    }

    /// Resolves once an instance found earlier is no longer found, and forgets
    /// the loss.
    pub(super) async fn lost(&self) {
        core::future::poll_fn(|cx| {
            self.waker.register(cx.waker());
            if self.lost.swap(false, Ordering::Relaxed) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }

    fn publish(&self, index: usize, found: bool) {
        if self.found[index].swap(found, Ordering::Relaxed) && !found {
            self.lost.store(true, Ordering::Relaxed);
            self.waker.wake();
        }
    }

    #[cfg(test)]
    pub(super) fn mark_found(&self, index: usize) {
        self.publish(index, true);
    }

    #[cfg(test)]
    pub(super) fn mark_lost(&self, index: usize) {
        self.publish(index, false);
    }
}

/// The sender holding an instance and what its last offer said.
#[derive(Debug, Clone, Copy)]
struct Holder {
    sender: SocketAddrV4,
    endpoint: NetEndpoint,
    /// Ticks left before the offer's TTL runs out; `None` for [`MAX_TTL`].
    ticks_left: Option<u32>,
}

/// The receive loop's state of one [`Target`].
#[derive(Debug, Clone, Copy, Default)]
struct Slot {
    holder: Option<Holder>,
    subscribed: bool,
}

/// What the receive loop knows of the instances in a [`Discovered`]; each
/// change is published there and reported to the [`ServiceObserver`].
pub(super) struct Discovery<'a> {
    discovered: &'a Discovered,
    slots: [Slot; FIND_TARGETS_CAP],
    observer: Option<(ServiceObserver, usize)>,
}

impl<'a> Discovery<'a> {
    pub(super) fn new(
        discovered: &'a Discovered,
        observer: Option<(ServiceObserver, usize)>,
    ) -> Self {
        Self {
            discovered,
            slots: [Slot::default(); FIND_TARGETS_CAP],
            observer,
        }
    }

    /// A discovery of the instances `config` finds, reporting nothing, whose
    /// [`Discovered`] lives for the rest of the test.
    #[cfg(test)]
    pub(super) fn leaked(config: &ServerConfig) -> Discovery<'static> {
        let discovered = std::boxed::Box::leak(std::boxed::Box::new(Discovered::new(config)));
        Discovery::new(discovered, None)
    }

    /// Records an offer of `service` from `sender` at `endpoint`, valid for
    /// `ttl` seconds; returns whether `sender` holds the instance, so its
    /// offer is the one to answer. An offer that names no endpoint, or that a
    /// sender other than the holder sends, changes nothing.
    pub(super) fn offer(
        &mut self,
        sender: SocketAddr,
        service: ServiceInstance,
        endpoint: Option<NetEndpoint>,
        ttl: u32,
    ) -> bool {
        let (SocketAddr::V4(sender), Some(endpoint), Some(index)) =
            (sender, endpoint, self.index(service))
        else {
            return false;
        };
        let ticks_left = (ttl != MAX_TTL).then(|| ttl.saturating_add(1));
        let slot = &mut self.slots[index];
        match &mut slot.holder {
            Some(holder) if holder.sender != sender => return false,
            Some(holder) if holder.endpoint == endpoint => {
                holder.ticks_left = ticks_left;
                return true;
            }
            holder => {
                *holder = Some(Holder {
                    sender,
                    endpoint,
                    ticks_left,
                });
            }
        }
        self.update(index, Some(ServiceChange::Available { service, endpoint }));
        true
    }

    /// Records a stop offer of `service` from `sender`; returns whether
    /// `sender` held the instance.
    pub(super) fn stop(&mut self, sender: SocketAddr, service: ServiceInstance) -> bool {
        let Some(index) = self.held_by(sender, service) else {
            return false;
        };
        self.release(index);
        true
    }

    /// Records that a Subscribe for `service` was sent to `sender`; a sender
    /// that does not hold the instance changes nothing.
    pub(super) fn subscribed(&mut self, sender: SocketAddr, service: ServiceInstance) {
        if let Some(index) = self.held_by(sender, service) {
            self.slots[index].subscribed = true;
            self.update(index, None);
        }
    }

    /// Ages every held offer by one [`TICK`] and releases those whose TTL ran
    /// out.
    pub(super) fn tick(&mut self) {
        for index in 0..self.discovered.targets.len() {
            let expired = match &mut self.slots[index].holder {
                Some(Holder {
                    ticks_left: Some(ticks),
                    ..
                }) => {
                    *ticks -= 1;
                    *ticks == 0
                }
                _ => false,
            };
            if expired {
                self.release(index);
            }
        }
    }

    /// Whether a held offer can run out, so [`Self::tick`] must run.
    pub(super) fn ticking(&self) -> bool {
        self.slots
            .iter()
            .filter_map(|slot| slot.holder)
            .any(|holder| holder.ticks_left.is_some())
    }

    fn index(&self, service: ServiceInstance) -> Option<usize> {
        self.discovered
            .targets
            .iter()
            .position(|target| target.find.service == service)
    }

    fn held_by(&self, sender: SocketAddr, service: ServiceInstance) -> Option<usize> {
        let index = self.index(service)?;
        self.slots[index]
            .holder
            .is_some_and(|holder| SocketAddr::V4(holder.sender) == sender)
            .then_some(index)
    }

    fn release(&mut self, index: usize) {
        self.slots[index] = Slot::default();
        let service = self.discovered.targets[index].find.service;
        self.update(index, Some(ServiceChange::Gone { service }));
    }

    fn update(&mut self, index: usize, change: Option<ServiceChange>) {
        let slot = self.slots[index];
        let found =
            slot.holder.is_some() && (slot.subscribed || !self.discovered.targets[index].consumed);
        self.discovered.publish(index, found);
        if let (Some(change), Some((observer, ctx))) = (change, self.observer) {
            observer(ctx, change);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sd_codec::SubscribeEventgroupRequest;
    use core::net::Ipv4Addr;
    use std::sync::Mutex;

    static SEEN: Mutex<std::vec::Vec<(usize, ServiceChange)>> = Mutex::new(std::vec::Vec::new());

    fn record(ctx: usize, change: ServiceChange) {
        SEEN.lock().unwrap().push((ctx, change));
    }

    fn seen(ctx: usize) -> std::vec::Vec<ServiceChange> {
        SEEN.lock()
            .unwrap()
            .iter()
            .filter(|(c, _)| *c == ctx)
            .map(|&(_, change)| change)
            .collect()
    }

    fn instance(service_id: u16) -> ServiceInstance {
        ServiceInstance::new(service_id, 1, 1).unwrap()
    }

    fn sender(host: u8) -> SocketAddr {
        SocketAddr::from(([192, 168, 0, host], 30490))
    }

    fn endpoint(port: u16) -> NetEndpoint {
        NetEndpoint::udp(SocketAddr::from(([192, 168, 0, 10], port)))
    }

    fn available(port: u16) -> ServiceChange {
        ServiceChange::Available {
            service: instance(0x1234),
            endpoint: endpoint(port),
        }
    }

    fn gone() -> ServiceChange {
        ServiceChange::Gone {
            service: instance(0x1234),
        }
    }

    fn consumed(service_id: u16, ttl: u32) -> SubscribeEventgroupRequest {
        SubscribeEventgroupRequest {
            service: instance(service_id),
            event_group_id: 1,
            ttl,
            local_ip: Ipv4Addr::LOCALHOST,
            local_rx_port: 40000,
        }
    }

    /// The instances a node requiring 0x1234 finds.
    fn requiring() -> &'static Discovered {
        let config = ServerConfig::consumer_only().with_required_service(instance(0x1234));
        std::boxed::Box::leak(std::boxed::Box::new(Discovered::new(&config)))
    }

    fn found(discovered: &Discovered) -> bool {
        discovered.unfound().next().is_none()
    }

    #[test]
    fn the_first_offer_is_available_and_found() {
        let discovered = requiring();
        let mut discovery = Discovery::new(discovered, Some((record, 1)));

        discovery.offer(sender(1), instance(0x1234), Some(endpoint(30501)), 3);

        assert_eq!(seen(1), [available(30501)]);
        assert!(found(discovered));
    }

    #[test]
    fn a_repeated_offer_reports_nothing() {
        let mut discovery = Discovery::new(requiring(), Some((record, 2)));

        discovery.offer(sender(1), instance(0x1234), Some(endpoint(30501)), 3);
        discovery.offer(sender(1), instance(0x1234), Some(endpoint(30501)), 3);

        assert_eq!(seen(2), [available(30501)]);
    }

    #[test]
    fn the_holder_offering_a_new_endpoint_is_available_again() {
        let mut discovery = Discovery::new(requiring(), Some((record, 3)));

        discovery.offer(sender(1), instance(0x1234), Some(endpoint(30501)), 3);
        discovery.offer(sender(1), instance(0x1234), Some(endpoint(30502)), 3);

        assert_eq!(seen(3), [available(30501), available(30502)]);
    }

    #[test]
    fn another_sender_is_ignored_while_one_holds_the_instance() {
        let mut discovery = Discovery::new(requiring(), Some((record, 4)));

        discovery.offer(sender(1), instance(0x1234), Some(endpoint(30501)), 3);
        discovery.offer(sender(2), instance(0x1234), Some(endpoint(30502)), 3);
        discovery.stop(sender(2), instance(0x1234));

        assert_eq!(seen(4), [available(30501)]);
    }

    #[test]
    fn after_the_holders_stop_any_sender_is_available() {
        let discovered = requiring();
        let mut discovery = Discovery::new(discovered, Some((record, 5)));

        discovery.offer(sender(1), instance(0x1234), Some(endpoint(30501)), 3);
        discovery.stop(sender(1), instance(0x1234));
        assert!(!found(discovered));
        discovery.offer(sender(2), instance(0x1234), Some(endpoint(30502)), 3);

        assert_eq!(seen(5), [available(30501), gone(), available(30502)]);
        assert!(found(discovered));
    }

    #[test]
    fn an_instance_not_found_by_the_node_is_ignored() {
        let discovered = requiring();
        let mut discovery = Discovery::new(discovered, Some((record, 6)));

        discovery.offer(sender(1), instance(0x5678), Some(endpoint(30501)), 3);
        let other_major = ServiceInstance::new(0x1234, 1, 2).unwrap();
        discovery.offer(sender(1), other_major, Some(endpoint(30501)), 3);
        discovery.stop(sender(1), instance(0x1234));

        assert!(seen(6).is_empty());
        assert!(!found(discovered));
    }

    #[test]
    fn an_offer_without_an_endpoint_is_not_found() {
        let discovered = requiring();
        let mut discovery = Discovery::new(discovered, Some((record, 7)));

        discovery.offer(sender(1), instance(0x1234), None, 3);

        assert!(seen(7).is_empty());
        assert!(!found(discovered));
    }

    #[test]
    fn an_offer_runs_out_after_its_ttl_and_one_more_tick() {
        let discovered = requiring();
        let mut discovery = Discovery::new(discovered, Some((record, 8)));
        discovery.offer(sender(1), instance(0x1234), Some(endpoint(30501)), 2);

        discovery.tick();
        discovery.tick();
        assert_eq!(seen(8), [available(30501)]);
        assert!(discovery.ticking());
        discovery.tick();

        assert_eq!(seen(8), [available(30501), gone()]);
        assert!(!discovery.ticking());
        assert!(!found(discovered));
    }

    #[test]
    fn a_renewal_restarts_the_ttl() {
        let mut discovery = Discovery::new(requiring(), Some((record, 9)));
        discovery.offer(sender(1), instance(0x1234), Some(endpoint(30501)), 2);
        discovery.tick();
        discovery.tick();

        discovery.offer(sender(1), instance(0x1234), Some(endpoint(30501)), 2);
        discovery.tick();
        discovery.tick();

        assert_eq!(seen(9), [available(30501)]);
    }

    #[test]
    fn an_offer_for_the_maximum_ttl_never_runs_out() {
        let mut discovery = Discovery::new(requiring(), Some((record, 10)));

        discovery.offer(sender(1), instance(0x1234), Some(endpoint(30501)), MAX_TTL);

        assert!(!discovery.ticking());
    }

    #[test]
    fn a_consumed_instance_is_found_once_subscribed_and_lost_when_gone() {
        let config = ServerConfig::consumer_only().with_consumed_eventgroup(consumed(0x1234, 5));
        let discovered = Discovered::new(&config);
        let mut discovery = Discovery::new(&discovered, None);

        discovery.offer(sender(1), instance(0x1234), Some(endpoint(30501)), 3);
        assert!(!found(&discovered), "offered but not subscribed");
        discovery.subscribed(sender(1), instance(0x1234));
        assert!(found(&discovered));
        discovery.stop(sender(1), instance(0x1234));
        discovery.offer(sender(1), instance(0x1234), Some(endpoint(30501)), 3);

        assert!(!found(&discovered), "a new offer needs a new Subscribe");
    }

    #[test]
    fn instances_are_found_consumed_first_each_once() {
        let config = ServerConfig::consumer_only()
            .with_required_service(instance(0x6001))
            .with_required_service(instance(0x5678))
            .with_required_service(instance(0x6001))
            .with_consumed_eventgroup(consumed(0x5678, 5));

        let targets: std::vec::Vec<_> = Discovered::new(&config)
            .unfound()
            .map(|target| (target.service.service_id(), target.ttl))
            .collect();

        assert_eq!(targets, [(0x5678, 5), (0x6001, REQUIRED_FIND_TTL)]);
    }

    #[test]
    fn losing_a_found_instance_wakes_the_finder() {
        use futures_util::FutureExt;
        let discovered = requiring();
        let mut discovery = Discovery::new(discovered, None);
        discovered.forget_losses();

        discovery.offer(sender(1), instance(0x1234), Some(endpoint(30501)), 3);
        assert!(
            discovered.lost().now_or_never().is_none(),
            "nothing lost yet"
        );
        discovery.stop(sender(1), instance(0x1234));

        assert!(discovered.lost().now_or_never().is_some());
        assert!(
            discovered.lost().now_or_never().is_none(),
            "a loss is reported once"
        );
    }
}
