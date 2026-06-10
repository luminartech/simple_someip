# Memory-footprint reduction: issue #125 + Phase 22 close-out

**Date:** 2026-06-09 (rev 2 — post adversarial review, same day)
**Base:** `feature/phase21_api_symmetry` (PR #114), after PR #124 merges into it
**Closes:** issue #125; completes the Phase 22 server alloc-elimination plan

## Problem

Issue #125 reports Embassy arena exhaustion in the consuming firmware
build (halo / TC4): `TaskStorage` entries for simple_someip tasks
dominate the arena, and static pool symbols are oversized. Two root
causes:

1. **Async state-machine bloat.** `Inner::run_future` is a single
   `select_biased!` loop that inlines the entire
   `handle_control_message` call tree (~370 lines, itself awaiting
   `bind_unicast` → socket spawn → send → oneshot recv). Rustc reserves
   layout for the sum of all nested awaited futures along the deepest
   path. The same pattern exists in `socket_loop_future` and the
   server's `recv_loop` / `announce_loop`.
2. **Buffers and pools held by value.**
   - `socket_manager.rs:569` holds a `[u8; UDP_BUFFER_SIZE]` (1500 B)
     live across the whole socket loop, and `socket_manager.rs:632`
     adds a second 1500 B buffer during E2E sends. With
     `UNICAST_SOCKETS_CAP = 8` (`inner.rs:40`), that is **~12 KiB
     always-live and ~24 KiB worst case** during concurrent E2E sends
     — all of it inside future state, i.e. inside the Embassy arena.
   - Static channel pools holding `SendMessage` / `ReceivedMessage`
     elements that embed full `Message<P>` payloads by value. The
     `define_static_channels!` macro already lets consumers tune
     **pool sizes** per type; what is hardcoded in crate code is the
     bounded **slot caps** (`C::bounded::<T, 16>()` call sites), the
     control queue `Deque<_, 32>`, and the pending-responses map (64).

Separately, the Phase 22 plan (make `server,bare_metal` build under
halo's `-Zbuild-std=core`, i.e. no alloc in the sysroot) gates any
on-target measurement of the server loops. PR #124 (Feliciano) has
since implemented most of Phase 22 — **verified 2026-06-09**: on
#124's branch, both `--features server,bare_metal` and
`--features client,server,bare_metal` compile clean under
`cargo +nightly build -Zbuild-std=core --target thumbv7em-none-eabihf`.

### Framing: arena vs. `.bss`

Moving buffers out of futures does **not** reduce total RAM — it moves
bytes from the Embassy arena (TaskStorage) into consumer-declared
statics (`.bss`), and shrinks them where the consumer right-sizes the
declarations. That is the correct fix for #125's failure mode: the
arena is the thing that exhausts, and `.bss` is sized explicitly and
predictably. Reviewers of the before/after tables should expect
TaskStorage entries to shrink while some static symbols grow or move
to the consumer's crate; the headline win is arena predictability
plus whatever the consumer saves by right-sizing.

## Decisions already made

- **Measurement:** in-repo harness for development and regression
  gating; the locally available TC4 build is the final acceptance
  check once hooked up. On-target "before" numbers remain capturable
  later by building the pre-optimization commit — baselines do not
  block on TC4 bring-up.
- **Scope:** everything in #125 (client, server, pools), with Phase 22
  folded into the same stack.
- **Client restructuring aggressiveness:** moderate — buffer
  extraction plus handler-tree flattening, staying in ordinary async
  Rust. No hand-written poll state machines (the polled module from
  PR #126 already serves users who need exact layouts).
- **Buffer sizing (rev 2):** buffers become **caller-sized**. Once
  extracted from the futures, socket loops take `&'static mut [u8]`
  slices, so the buffer count and length are chosen by the consumer's
  static declaration at runtime-slice granularity — no const-generic
  threading through `Client`'s parameters. Halo can declare e.g.
  2 × 512 B = 1 KiB instead of 12 KiB. The tokio path provisions
  8 × 1500 internally (API and behavior unchanged).

  *512 B rationale (sized against Iris generic interface 0.11):* the
  largest defined payload is `SoftwareApplicationInfo` at 256 B
  (≈284 B on-wire with SOME/IP + E2E P04 headers); `ScanCmd` is 88 B.
  ScanCmd's command list is the growth risk (`uint16` length field),
  but the crate's hard ceiling is already one UDP datagram
  (`UDP_BUFFER_SIZE = 1500`, no SOME/IP-TP), so nothing larger was
  ever sendable; outgrowing 512 B is a logged drop fixed by a
  one-line bump in the consumer's declaration. Any halo-side traffic
  beyond the generic interface (e.g. HWP1 method requests) needs the
  same size check before the declaration is locked.
- **Pool capacities:** narrowed from "promote everything to
  const-generic knobs". Pool sizes are already consumer-tunable via
  `define_static_channels!`. The remaining hardcoded numbers (slot
  caps 16, `Deque<_, 32>`, pending map 64) can only become knobs on
  stable Rust as **literal const parameters threaded through
  `Client`/`Inner`'s public types** (associated-const capacities at
  the call sites would require unstable `generic_const_exprs`). That
  churn is taken only where PR 0/TC4 measurement shows the win
  justifies it; otherwise the numbers stay hardcoded and the decision
  is recorded.
- **PR #124:** merges as-is; our review findings are fixed by us in
  this stack rather than requested from the author.
- **Stack hygiene:** all 37 stale phase PRs beneath #114 were closed
  without merging on 2026-06-09 (branches retained).

## PR #124 coverage of Phase 22 (reviewed 2026-06-09)

| Phase 22 item | Status in #124 |
|---|---|
| Item 4 — `_alloc`-gate `Server::run` | Done (`run` + `run_inner`) |
| Item 5 — remove `Pin<Box<dyn Future>>` GATs | Done via `core::future::Ready` (simpler than the planned hand-written futures; supersedes the saved pre-flight patch) |
| Item 2 — started latch without `Arc` | Done via cfg-switched `StartedLatch` alias instead of an `Hstart` generic |
| Item 3 — Arc type-param defaults | Done via cfg-switched `Default*Handle` aliases instead of dropping defaults |
| Items 1+10 — import reshape, `server` feature drops `_alloc` | Done |
| CI gate `server,bare_metal -Zbuild-std=core` | **Missing** (gap-filled in PR 0; verified locally that it passes) |

**Why the existing CI doesn't already cover this:** phase21's CI does
build `server,bare_metal` for thumbv7em-none-eabihf — but against the
**prebuilt sysroot, which ships `alloc`**, so an `extern crate alloc`
regression would never E0463 there. Halo's proxy builds with
`-Zbuild-std=core`, where `alloc` is absent from the sysroot entirely.
PR 0's build-std job is the only configuration that certifies halo's
actual constraint. Relatedly, the existing `nm` alloc-symbol audit
covers only the `client,bare_metal` rlib; PR 0 extends it to
`server,bare_metal` (stable-toolchain, nearly free).

**Accepted trade-off:** the cfg-switched aliases violate strict feature
additivity (enabling `_alloc` changes type identities). This is
documented as a hazard rather than redesigned — halo builds with a
fixed feature set, and explicit `new_with_handles` callers spell their
types. The generic-parameter design remains available if a real
unification break ever appears.

**Pre-flight note:** the Phase 22 plan's open risk — whether a concrete
(non-boxed) future satisfies `Server::run`'s phase-21F
`for<'a> Sub::SubscribeFuture<'a>: Send` HRTB — was verified resolved
on 2026-06-09 against phase21 tip `892cb5b`. `core::future::Ready`
satisfies the same bounds.

## The stack

Four PRs off `feature/phase21_api_symmetry`, post-#124.

### PR 0 — Measurement harness + CI gates

- **Future-size regression tests:** `size_of_val`-based assertions on
  client `run_future`, `socket_loop_future`, and the server run
  future, in both tokio and bare-metal-deps configurations (modeled on
  the existing `client_new_run_future_is_send_static` witness, which
  already returns the run future by value). These are **host-arch
  proxies**: x86_64 layouts differ from thumbv7 (pointer width,
  alignment), so budgets are generous regression tripwires, not
  targets. Budgets start at current size (recording the baseline) and
  tighten as PRs 2–3 land.
- **`-Z print-type-sizes` capture script** in `tools/`, producing the
  TaskStorage-style table for a thumbv7em build. This is the
  **authoritative** size number. Baseline committed.
- **New CI jobs:** `--no-default-features --features server,bare_metal`
  and `client,server,bare_metal` under `-Zbuild-std=core` for
  thumbv7em (nightly + rust-src — new CI infrastructure; the current
  workflows are stable-only). Verified passing locally on #124's
  branch. Plus the `nm` alloc-symbol audit extended to the server
  rlib.

### PR 1 — #124 follow-ups (small, lands the breaking change early)

The #124 review findings, fixed by us:

- (a) Document (or deliberately change) the eager-vs-lazy semantics of
  the `Ready`-based `subscribe`/`unsubscribe` — the locked mutation now
  happens at future construction, not first poll.
- (b) `NonSdRequestCallback` gains a context argument. **Design note:**
  a stored `*mut c_void` would make `Server` `!Send` and break
  `Server::run`'s declared `+ Send` bound. The shape is decided in the
  implementation plan from: `ctx: usize` (caller casts), a newtype
  with a documented `unsafe impl Send`, or a generic observer
  parameter. Breaking now is free (nothing published); breaking later
  is not. Flag to Feliciano before this lands so no further FFI builds
  on the bare `fn` shape.
- (c) Record the shared-socket-topology rationale for
  `announce_only_future` (it partially reintroduces the split-future
  shape phase 21 removed). The originally-planned MSRV check is moot:
  the crate is edition 2024 (requires Rust ≥ 1.85); `use<>` precise
  capture needs only 1.82.
- (d) Strengthen the non-SD-observer negative test (currently cannot
  fail — the witness callback is never registered).

### PR 2 — #125 client async-state reduction

- **Buffer extraction via a claim/release buffer pool.** Socket loops
  are spawned dynamically per bind/unbind (up to
  `UNICAST_SOCKETS_CAP` live), so buffers need checkout/return
  semantics: a buffer pool in the consumer's static storage (same
  shape as `OneshotPool` — claim on bind, release on unbind, no
  `&'static mut` aliasing). Loops take `&'static mut [u8]` slices;
  count and length are the consumer's choice (halo: ~1 KiB total).
  The tokio path provisions 8 × 1500 internally — API unchanged.
  Defined behavior changes: an inbound datagram larger than the
  claimed buffer is dropped with a log; the existing oversize-send
  rejection (`socket_manager.rs:447`) checks `buf.len()` instead of
  `UDP_BUFFER_SIZE`. The E2E `protected` buffer gets the same pool
  treatment, or is restructured to not be live across the
  `protect().await` point — whichever measures better.
- **Handler-tree flattening:** `handle_control_message` splits into a
  synchronous "decode + decide" section returning a small action
  value; the actual awaits are hoisted to shallow helpers at
  `run_future`'s top level. **Expectation setting:** the awaited
  futures remain part of `run_future`'s layout — the wins are locals
  no longer held across awaits, avoided per-nesting-level argument
  duplication, and better variant overlap. The buffers are expected to
  be the dominant win; flattening is secondary and is kept only where
  PR 0's numbers move.
- Doc debt: rewrite `src/client/mod.rs:12-30` (describes the old
  buffer-in-future architecture and the 12 KiB math).
- Every change is validated against PR 0's numbers; changes that don't
  move the measurement are dropped, not merged on faith.
- **Deferred follow-up (recorded, not scheduled):** a readiness-split
  receive (`await readiness, then synchronous copy-out`) would let one
  shared buffer serve all socket loops on a single-threaded executor
  (~1.5 KiB total regardless of socket count). It requires a
  `TransportSocket` trait change; only worth it if caller-sizing
  proves insufficient.

### PR 3 — #125 server + pools, final numbers

- Same flatten/extract treatment for `recv_loop`, `announce_loop`,
  `send_subscribe_nack_from_view`, on the post-#124 code.
- Pool-capacity knobs per the narrowed decision above (literal const
  params only where measurement justifies the churn; otherwise record
  and keep).
- Final before/after tables (TaskStorage sizes + `llvm-nm` pool
  symbols) in the PR description — issue #125's acceptance criteria.

### Scope cuts from issue #125 (recorded)

- **SD encode monomorphization** (`Header::encode`,
  `ServiceEntry::encode`, `EventGroupEntry::encode`, generic over
  `embedded_io::Write`): code-size pressure, not arena/RAM pressure.
  Out of scope for the arena-exhaustion failure mode. If flash size
  becomes the constraint, the cheap fix is an inner non-generic
  `&mut dyn embedded_io::Write` function — separate issue.
- **`unbind_discovery`:** addressed only implicitly via the
  `run_future` flattening in PR 2 (it is one arm of the same control
  path); no dedicated work item unless PR 0's table shows it as an
  independent hotspot.

## Invariants

- No behavioral changes except the agreed `NonSdRequestCallback`
  signature change and the two defined buffer-size behaviors in PR 2.
- Existing suite (~543 tests as of #114, plus #124's additions) green
  on every PR; embassy-net loopback live-wire test guards the announce
  path.
- No nightly-only features in the crate itself (halo's consumer is
  nightly; the crate stays stable; CI may use nightly for measurement
  and build-std jobs).
- Wire format untouched.

## Risks / coordination

- **#126 (polled module)** is Feliciano's, has an outstanding
  hold-merge punch list, and also bases on phase21. Merge order
  relative to this stack is decided between Justin and Feliciano; the
  polled module is a parallel surface, so PRs 2–3 should rebase
  trivially either way.
- **#124 is force-pushed actively.** Our stack starts only after it
  merges into phase21, to avoid chasing a moving base.
- **Future-size assertions can be brittle across rustc versions** and
  are host-arch proxies. Budgets use generous headroom (e.g. +25%)
  over the post-optimization baseline; the thumbv7em
  `print-type-sizes` harness is the authoritative number.
- **The buffer pool's claim/release lifecycle** is new unsafe-adjacent
  surface (handing out `&'static mut [u8]`); the implementation plan
  includes loom-style or witness tests for double-claim and
  release-on-unbind.
- The whole stack still sits on the unmerged #114 tower
  (134+ commits ahead of main); the eventual consolidation rebase is a
  known cost of the established workflow, accepted to keep reviewable
  PR boundaries.
