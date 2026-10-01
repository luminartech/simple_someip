//! Interop tests for the bare-metal runtime against vsomeip and spec-derived frames.
#![cfg(all(target_os = "linux", feature = "bare-metal-runtime"))]

mod interop;

type Rt = interop::runtimes::bare_metal_rt::BareMetalRt;
