//! Reusable bare-metal SOME/IP runtime.
//!
//! A platform (e.g. an AURIX/lwIP firmware) drives the full SOME/IP
//! integration by supplying only:
//! - its **generated catalog** (offered services, which may be none, an
//!   optional subscription, and the services it requires),
//! - **platform callbacks**: send a UDP datagram, read a ms clock, a
//!   dispatch sink for inbound messages, and an optional sink for the
//!   availability of required services,
//! - **RX delivery**: the platform's receive path pushes datagrams into an
//!   [`crate::bare_metal_runtime::RxMailbox`] this runtime polls, and
//! - **buffer memory** it owns (so the platform controls link placement).
//!
//! Everything else lives here: the SD codec, subscribe-accept, the
//! combined-offer announce, `FindService` and answering Offers with
//! Subscribes, tracking and expiring the Offers of required services,
//! notification dispatch, E2E, and the embassy executor + task.

mod mailbox;
mod transport;

pub use mailbox::{RxMailbox, RxSlot};
pub use transport::{CallbackFactory, CallbackSocket, CallbackTimer, NowMsFn, Platform, SendFn};

#[cfg(feature = "bare-metal-runtime")]
mod runtime;
#[cfg(feature = "bare-metal-runtime")]
pub use crate::sd_availability::ServiceChange;
#[cfg(feature = "bare-metal-runtime")]
pub use runtime::{
    BindFn, DispatchFn, OfferEntry, RX_CAP, RX_SLOTS, RequiredEntry, RuntimeBuffers, RuntimeConfig,
    ServiceFn, SubEntry, deinit, init, on_rx, poll, publish,
};
