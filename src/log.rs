//! Internal log-macro shim. Crate-private — never re-exported.
//!
//! When the `tracing` feature is on, `crate::log::{debug, error, info,
//! trace, warn}` re-export the corresponding `tracing::*` macros
//! verbatim. When off (`bare_metal`-without-`std` builds, where the
//! `tracing-core` `extern crate alloc` declaration would fail against
//! a `core`-only sysroot), the names resolve to a single token-eating
//! macro that wraps `core::format_args!` in an `if false { … }` block:
//! references in the format string still count as variable uses for
//! the borrow checker (no spurious `unused_variables` lints in callers
//! that only consume a binding inside a log call), and the dead block
//! optimizes out, so no log code reaches the linker.

// `unused_imports` because rustc only counts a macro re-export as used
// when it appears in a `use crate::log::name` path; bare-macro
// invocations (`crate::log::name!(…)`) are resolved through the macro
// table rather than the item table and don't satisfy the lint. Three
// of these (debug/error/info) happen not to appear in any `use`-list
// today, so they trip an unused-import warning that doesn't reflect
// reality. Suppress here rather than restructure the call sites.
#[cfg(feature = "tracing")]
#[allow(unused_imports)]
pub(crate) use tracing::{debug, error, info, trace, warn};

#[cfg(not(feature = "tracing"))]
macro_rules! noop {
    ($($arg:tt)+) => {
        if false {
            let _ = ::core::format_args!($($arg)+);
        }
    };
}

#[cfg(not(feature = "tracing"))]
pub(crate) use noop as debug;
#[cfg(not(feature = "tracing"))]
pub(crate) use noop as error;
#[cfg(not(feature = "tracing"))]
pub(crate) use noop as info;
#[cfg(not(feature = "tracing"))]
pub(crate) use noop as trace;
#[cfg(not(feature = "tracing"))]
pub(crate) use noop as warn;
