//! Interop scenarios, shared by both test targets.

/// `scenario!(name, std = run, bare_metal = ignore("reason"), { body })`
///
/// Expands to one `#[test] fn name()` that runs the loopback preflight check
/// and then `body`. Each runtime takes exactly `run` or `ignore("reason")`;
/// anything else is a compile error. Leading attributes (such as the doc
/// comment naming the requirement) are kept on the test function. Scenario
/// modules declared below this definition see it by textual scope.
macro_rules! scenario {
    ($(#[$attr:meta])* $name:ident, std = $($rest:tt)*) => {
        scenario!(@std [$(#[$attr])*] $name; $($rest)*);
    };
    (@std [$($a:tt)*] $name:ident; run, bare_metal = $($rest:tt)*) => {
        scenario!(@bare_metal [$($a)*] $name; $($rest)*);
    };
    (@std [$($a:tt)*] $name:ident; ignore($r:literal), bare_metal = $($rest:tt)*) => {
        scenario!(@bare_metal
            [$($a)* #[cfg_attr(not(feature = "bare-metal-runtime"), ignore = $r)]]
            $name; $($rest)*);
    };
    (@bare_metal [$($a:tt)*] $name:ident; run, $body:block) => {
        scenario!(@emit [$($a)*] $name; $body);
    };
    (@bare_metal [$($a:tt)*] $name:ident; ignore($r:literal), $body:block) => {
        scenario!(@emit [$($a)* #[cfg_attr(feature = "bare-metal-runtime", ignore = $r)]] $name; $body);
    };
    (@emit [$($a:tt)*] $name:ident; $body:block) => {
        $($a)*
        #[test]
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
