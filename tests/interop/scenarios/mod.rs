//! Interop scenarios, shared by both test targets.

/// `scenario!(name, std = run, bare_metal = ignore("reason"), { body })`
///
/// Expands to one `#[test] fn name()` that runs the loopback preflight check
/// and then `body`. `ignore("reason")` ignores the test on that runtime; `run`
/// runs it. Leading attributes (such as the doc comment naming the
/// requirement) are kept on the test function. Scenario modules declared below
/// this definition see it by textual scope.
macro_rules! scenario {
    (
        $(#[$attr:meta])*
        $name:ident,
        std = $s:ident $(($sr:literal))?,
        bare_metal = $b:ident $(($br:literal))?,
        $body:block
    ) => {
        $(#[$attr])*
        #[test]
        $( #[cfg_attr(not(feature = "bare-metal-runtime"), ignore = $sr)] )?
        $( #[cfg_attr(feature = "bare-metal-runtime", ignore = $br)] )?
        fn $name() {
            crate::interop::preflight();
            $body
        }
    };
}

use std::time::Duration;

/// Long enough for an initial offer plus a couple of cyclic repetitions.
pub const SD_WAIT: Duration = Duration::from_secs(6);
/// The 3 s TTL plus margin.
pub const TTL_WAIT: Duration = Duration::from_secs(5);
/// How long to watch for something that must not happen.
pub const QUIET: Duration = Duration::from_millis(1500);
/// How long a method call may take.
pub const CALL_WAIT: Duration = Duration::from_secs(3);

pub mod discovery;
