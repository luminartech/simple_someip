//! The runtime-neutral interface every interop scenario drives. Each runtime
//! under test (the std `Client`/`Server` and the bare-metal runtime) has an
//! adapter in `runtimes/` that implements [`SomeipUnderTest`].

use std::net::SocketAddrV4;
use std::time::{Duration, Instant};

/// What the runtime under test does when it starts.
#[derive(Debug, Clone)]
pub struct Setup {
    pub offer: Option<Offer>,
    pub consume: Option<Consume>,
}

/// Offer `service`/`INST` with the given TTL, events, fields and methods.
#[derive(Debug, Clone)]
pub struct Offer {
    pub service: u16,
    pub ttl_s: u32,
    pub events: Vec<u16>,
    pub fields: Vec<(u16, Vec<u8>)>,
    pub methods: Vec<u16>,
}

/// Look for `SVC`/`INST`, optionally subscribing to `EG` once it is found.
#[derive(Debug, Clone)]
pub struct Consume {
    pub subscribe: bool,
    pub e2e: Option<E2eSpec>,
}

/// E2E Profile 5 parameters for the consumed event.
#[derive(Debug, Clone)]
pub struct E2eSpec {
    pub data_id: u16,
    /// The protected data's length in bits, as the specification states it.
    pub data_length_bits: u16,
    pub max_delta: u8,
}

/// Something the runtime under test reported. A `None` field is one the
/// runtime does not expose.
#[derive(Debug, Clone)]
pub enum Observation {
    ServiceAvailable {
        service: u16,
        instance: u16,
        endpoint: SocketAddrV4,
    },
    ServiceGone {
        service: u16,
        instance: u16,
    },
    /// The result of the runtime's own subscription to `eventgroup`, as
    /// the runtime reports it. Adapters never derive it from SD entries
    /// seen on the wire.
    Subscribed {
        eventgroup: u16,
        accepted: bool,
    },
    Event {
        service: u16,
        event: u16,
        payload: Vec<u8>,
        e2e_ok: Option<bool>,
    },
    Request {
        service: u16,
        method: u16,
        no_return: Option<bool>,
        session: Option<u16>,
        payload: Vec<u8>,
    },
    /// An error the runtime reported on its own (not in reply to a call).
    RuntimeError {
        message: String,
    },
}

/// The result of a method call made by the runtime under test. A `None`
/// field is one the runtime does not expose.
#[derive(Debug, Clone)]
pub enum CallOutcome {
    Response {
        return_code: Option<u8>,
        payload: Vec<u8>,
    },
    Error {
        return_code: u8,
    },
    NoReply,
}

/// A SOME/IP runtime under test.
pub trait SomeipUnderTest: Sized {
    const NAME: &'static str;

    fn start(setup: Setup) -> Self;
    fn stop_offer(&mut self);
    fn publish(&mut self, event: u16, payload: &[u8]);
    fn set_field(&mut self, field: u16, payload: &[u8]);
    fn unsubscribe(&mut self);
    fn call(&mut self, method: u16, payload: &[u8], timeout: Duration) -> CallOutcome;
    fn fire_and_forget(&mut self, method: u16, payload: &[u8]);

    /// The next observation, or `None` if there is none within `timeout`.
    fn next(&mut self, timeout: Duration) -> Option<Observation>;

    /// Waits for an observation matching `f`, skipping others; panics with
    /// everything it skipped if none arrives within `timeout`.
    fn expect(
        &mut self,
        what: &str,
        timeout: Duration,
        f: impl Fn(&Observation) -> bool,
    ) -> Observation {
        let deadline = Instant::now() + timeout;
        let mut seen = Vec::new();
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.next(left) {
                Some(o) if f(&o) => return o,
                Some(o) => seen.push(format!("{o:?}")),
                None => break,
            }
        }
        panic!(
            "{}: expected {what} within {timeout:?}; observed: {seen:?}",
            Self::NAME
        )
    }

    /// Panics if an observation matching `f` arrives within `within`.
    fn expect_none(&mut self, what: &str, within: Duration, f: impl Fn(&Observation) -> bool) {
        let deadline = Instant::now() + within;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.next(left) {
                Some(o) if f(&o) => panic!("{}: unexpected {what}: {o:?}", Self::NAME),
                Some(_) => {}
                None => break,
            }
        }
    }
}

/// Fails a scenario that needs a capability `runtime` does not have.
pub fn unsupported(runtime: &str, capability: &str) -> ! {
    panic!("unsupported on {runtime}: {capability}")
}
