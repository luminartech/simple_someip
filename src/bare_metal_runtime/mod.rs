//! Reusable bare-metal SOME/IP runtime.
//!
//! A platform (e.g. an AURIX/lwIP firmware) drives the full SOME/IP
//! integration by supplying only:
//! - its **generated catalog** (offered services + subscriptions),
//! - **platform callbacks**: send a UDP datagram, read a ms clock, and a
//!   dispatch sink for inbound messages,
//! - **RX delivery**: the platform's receive path pushes datagrams into an
//!   [`RxMailbox`] this runtime polls, and
//! - **buffer memory** it owns (so the platform controls link placement).
//!
//! Everything else — the SD codec, subscribe-accept, the combined-offer
//! announce, the proactive subscribe, notification dispatch, E2E, and the
//! embassy executor + task — lives here.
//!
//! This phase exposes the callback transport and the mailbox; the executor
//! + task + C-ABI land in later submodules.

mod mailbox;
mod transport;

pub use mailbox::{RxMailbox, RxSlot};
pub use transport::{
    CallbackFactory, CallbackSocket, CallbackTimer, NowMsFn, Platform, SendFn,
};

#[cfg(feature = "bare-metal-runtime")]
mod runtime;
#[cfg(feature = "bare-metal-runtime")]
pub use runtime::{
    BindFn, DispatchFn, OfferEntry, RuntimeBuffers, RuntimeConfig, SubEntry, deinit, init, on_rx,
    poll, publish, RX_CAP, RX_SLOTS,
};
