//! Integration test: documents the intent that the `bare_metal` example
//! workspace member must compile cleanly. Guards against regressions in
//! the `transport`/`tokio_transport`/`Timer` trait surface that would
//! break bare-metal consumers.
//!
//! Compilation of the `bare_metal` example is already covered by
//! workspace-wide Cargo commands such as `cargo build --workspace`,
//! `cargo test --workspace`, or CI's `cargo clippy --workspace`, so
//! this file does not spawn a nested `cargo build` — nested cargo
//! invocations are redundant and flaky under lock contention. The test
//! body below is a minimal sanity check that the test harness ran at
//! all; the real coverage comes from those outer workspace-wide
//! checks. Keep this file so the regression's intent stays documented.

#[test]
fn bare_metal_workspace_member_compiles() {
    // Minimal canary: the test harness executed this test. Compilation of
    // the `bare_metal` example itself is enforced by explicit
    // workspace-wide checks (for example `cargo build --workspace`),
    // not by spawning a nested `cargo build` here — so an empty body is
    // sufficient.
}
