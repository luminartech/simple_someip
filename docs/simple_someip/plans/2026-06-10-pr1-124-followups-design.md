# PR 1 — #124 follow-ups: design

Second PR of the #125 / phase-22 close-out stack
(`2026-06-09-phase22-125-memory-reduction-design.md`, "PR 1" section).
Closes the four review findings recorded there against PR #124, plus
one stale-doc item surfaced during PR 0.

**Branch:** `feature/pr1_124_followups` off
`feature/pr0_measurement_harness` (PR base = the PR 0 branch). PR 1
must stack on PR 0: changing the callback shape touches `ServerDeps`
initializers in test files PR 0 modified
(`tests/bare_metal_e2e.rs`, `tests/bare_metal_server.rs`).

**External gate:** Feliciano gets the callback-signature heads-up
before this merges, so no further halo FFI builds on the bare `fn`
shape (stack-plan gate 2 — user action, still open as of
2026-06-10).

## 1. `NonSdRequestCallback` gains a context argument (BREAKING)

Decision (2026-06-10): **`ctx: usize`**, over an unsafe-Send
`*mut c_void` newtype and over a generic observer type parameter.

```rust
pub type NonSdRequestCallback =
    fn(ctx: usize, data: &[u8], source: core::net::SocketAddrV4);
```

- Storage everywhere becomes `Option<(NonSdRequestCallback, usize)>`
  — a plain tuple, no wrapper struct: the `Server` field,
  `ServerDeps.non_sd_observer`, and
  `ServerConfig::with_non_sd_observer`. `recv_loop` threads the pair
  down and invokes `cb(ctx, data, src)`.
- Rationale (recorded on the type alias's doc comment): a stored
  `*mut c_void` makes `Server` `!Send` and breaks `Server::run`'s
  declared `+ Send` bound; `usize` is trivially `Send + Sync`,
  keeps the field `Copy`, and matches the `uintptr_t` the C caller
  holds anyway. halo passes `(dispatch, state_ptr as usize)`;
  Rust-native users pass `(f, 0)`.
- **No `unsafe` enters this crate.** The library stores, copies, and
  passes back a plain integer through a safe `fn`-pointer call —
  `Server: Send` holds by construction, with no `unsafe impl` and no
  soundness contract for the library to document or uphold. The
  unsafe dereference (`ctx as *mut T`, then `unsafe { &*ptr }`)
  happens in the consumer's callback body — halo's FFI dispatch
  code, which is already unsafe territory and is the only party
  that can verify the pointee's lifetime and thread-safety. Known
  trade-off: `usize` carries no provenance, so the compiler can't
  stop a caller passing a wrong address — but the rejected
  `*mut c_void` newtype was equally untyped; only the
  generic-observer design would have fixed that, at the
  8th-type-parameter cost.
- Rejected alternatives, for the record: the unsafe-Send newtype
  moves an unverifiable soundness contract into this library; the
  generic observer adds an 8th type parameter that ripples through
  `ServerDeps`/`ServerHandles`/both cfg-switched alias families,
  and halo's FFI would still write its own unsafe-Send wrapper.
- Breaking now is free: 0.8.0 is unpublished and halo is the only
  consumer. CHANGELOG gets a breaking-change entry with the
  before/after signature.

## 2. Eager-`Ready` timing documented (no code change)

Decision (2026-06-10): document, don't make lazy.
`StaticSubscriptionHandle::subscribe`/`unsubscribe` execute the
locked mutation when the future is *constructed* (inside
`core::future::ready(...)`), unlike the `Box::pin(async)` impls,
which are lazy. The only in-tree caller (`runtime.rs`) awaits
immediately, so laziness would be ~80 lines of poll boilerplate for
behavior no current caller observes.

- Trait-level note on `SubscriptionHandle::subscribe`/`unsubscribe`:
  implementations with a fully synchronous critical section may
  perform the mutation at future construction; callers must not
  assume construction is side-effect-free.
- Matching sentence on the `StaticSubscriptionHandle` impl block.
- The phase-22 preflight patch's hand-written `StaticSubscribeFuture`
  (`.claude/phase22_item5_preflight.patch` in the main worktree) is
  permanently obsolete.

## 3. `announce_only_future` rationale (doc-only)

The method's doc already explains the shared-socket topology
(supplementary Servers via `new_with_handles` announce on the shared
SD socket; the primary owns all inbound loops). It gains the honest
acknowledgment that this partially reintroduces the split-future
shape phase 21 removed, and why that is acceptable here: an
announce-only future never touches the recv path, so the
single-run-future invariant that motivated phase 21b (no two futures
racing on the same sockets/session counter) is preserved — the
`started` latch still guards the full run-future.

The originally-planned MSRV check is recorded as moot: the crate is
edition 2024 (Rust ≥ 1.85); `use<>` precise capture needs only 1.82.

## 4. Strengthen the non-SD-observer negative test

`non_sd_observer_none_preserves_ignore_behavior`
(`tests/bare_metal_server.rs`) cannot currently fail: `record_none`
is never wired into the server, so no code path can populate
`OBSERVED_NONE` regardless of any routing regression.

Replace with negative tests that have a live witness — register the
recording callback as a real observer, then assert it does NOT fire
for:

- (i) an SD unicast datagram (exercises the SD-vs-non-SD routing
  branch), and
- (ii) a non-SD **multicast** datagram (exercises the
  unicast-vs-multicast branch — the mock needs to mark the datagram
  as arriving on the SD/multicast socket; mechanics resolved in the
  implementation plan).

The `None` case shrinks to what it actually proves: the run loop
processes a non-SD unicast datagram without panicking when no
observer is registered. The positive test
(`non_sd_observer_some_receives_unicast_method_request`) is updated
for the new signature and asserts the ctx value round-trips.

## 5. Stale `UDP_BUFFER_SIZE` rustdoc

Verified false during PR 0 review (2026-06-10): `send_ack`
(`src/server/runtime.rs`) builds into a stack
`[0u8; crate::UDP_BUFFER_SIZE]`, and the `server,bare_metal` rlib
audits to zero allocator symbols. The constant's rustdoc paragraph
claiming announcement builders / `SubscribeAck`/`Nack` "still use
heap `Vec` buffers — known gap" is rewritten: all outbound SD paths
are stack-buffered and capped by `UDP_BUFFER_SIZE`.

## Error handling

No new fallible paths. The callback invocation remains
fire-and-forget from `recv_loop` (a misbehaving callback is the
consumer's responsibility — same contract as today, restated in the
type-alias docs).

## Verification

fmt, clippy `--workspace --all-features` + `--no-default-features`
(both `-D warnings -D clippy::pedantic`), full suite at
`--test-threads=1`, doc tests, the three `-Zbuild-std=core` thumb
builds, and the `nm` server audit — the latter two also enforced in
CI since PR 0. Future-size witnesses from PR 0 must stay within
budget (the tuple adds 2×usize to the run-future capture; budgets
have 25% headroom).
