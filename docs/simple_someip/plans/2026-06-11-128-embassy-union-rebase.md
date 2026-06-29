# PR #128 Rebase — Union Callback Adoption Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rebase `feat/embassy-mem-channel-cap` (PR #128, Feliciano's draft) onto the merged #124→#127→#129 spine, dropping its three #124-duplicate commits and adopting the union `NonSdRequestCallback` contract for the runtime's `DispatchFn`, then capture the fresh size baseline that becomes PR 2's "before".

**Architecture:** A 9-commit rebase with a small, fully-enumerated conflict surface (dry-run executed 2026-06-11 — see inventory below), followed by a mechanical union-adoption pass on the new `bare_metal_runtime` files (2 compile errors, both at `runtime.rs:345`), a `DISPATCH_CTX` parallel static, and ctx/source threading through `event_rx_dispatch_future`.

**Tech Stack:** git rebase, Rust (nightly for the runtime's `#![feature]` and `-Zbuild-std`), cargo, GNU `nm`.

**Ownership note:** `feat/embassy-mem-channel-cap` is Feliciano's draft PR. This plan produces a **preview branch**; force-pushing his branch happens only with his explicit ack (Task 8).

---

## Dry-run inventory (executed 2026-06-11, preview preserved)

A complete dry run exists: branch `preview/embassy_union_rebase` (tip `35ba14f`) in worktree `/tmp/embassy_rebase_preview`, rebased onto the PR 1 tip `8e41b0e`. Tasks 1–2 below are **already done on that branch** — an executor can resume from it (start at Task 3) or replay from scratch using the recipes. Per-commit results:

| #128 commit | Result on rebase |
|---|---|
| `0901274`, `45a4630`, `1a4ca83` | **Dropped by construction** (rewritten duplicates of #124's `be292bb`/`3dafd3e`/`64fcc08`; patch-ids drifted so git will NOT auto-skip them — rebase from `1a4ca83`, not from the branch base) |
| `16e5b27` pre-bound sockets | clean |
| `1124531` host rx notify | clean |
| `7623829` payload/config sizes | clean |
| `63e549f` CLIENT_SOCKET_CHANNEL_CAP | 1 trivial conflict: `src/client/socket_manager.rs` import adjacency — keep BOTH lines |
| `0f48c16` ARENA cap cuts | clean |
| `d56a691` co-offered Subscribe | 1 conflict: `src/server/runtime.rs` — new `accept_subscribe` fn inserted above `handle_sd_message`; take embassy's insertion AND the chain's backticked doc line (`` `FindService` ``) |
| `77cf725` SD codec + helpers | 5 hunks in `src/server/mod.rs` / `src/server/runtime.rs` / `tests/bare_metal_server.rs` — ALL are union-vs-4-param of the same content; **keep HEAD (the union side) in every hunk**. The commit's new files (`src/sd_codec.rs`, helper fns) apply clean |
| `3f7d7d1` run_someip | clean |
| `223bf40` reusable runtime | clean (new files) — but **semantically un-adopted**: leaves exactly 2 compile errors at `src/bare_metal_runtime/runtime.rs:345` (E0308 `Some(fn)` vs `Option<(fn, usize)>`, E0605 4-param→6-param fn cast). Tasks 3–5 fix this |

---

### Task 1: Preconditions and base selection

**Files:** none (git only)

- [ ] **Step 1: Pick the rebase target**

The real target is `feature/phase21_api_symmetry` AFTER the spine (#124 → #127 → #129) merges. Until then, the PR 1 tip is content-identical: `origin/feature/pr1_124_followups` (`8e41b0e`). The preserved preview was built against the PR 1 tip. If the spine has merged since, replay Task 2 against the merged phase21 instead of resuming the preview — the inventory above still applies unless #129 was amended in review (check: `git log 8e41b0e..origin/feature/phase21_api_symmetry --oneline -- src/server/` — if #129 landed with changes beyond `8e41b0e`, re-verify the `77cf725` resolutions).

- [ ] **Step 2: Resume or replay?**

Run: `git -C /tmp/embassy_rebase_preview log --oneline -1 2>/dev/null`
If it prints `35ba14f feat(bare-metal): reusable runtime …`, resume from the preview (skip Task 2). Otherwise replay Task 2.

---

### Task 2: The rebase (replay recipe — skip if resuming the preview)

**Files:** conflict resolutions only, per the inventory.

- [ ] **Step 1: Create the worktree and rebase from above the duplicates**

```bash
git worktree add /tmp/embassy_rebase_preview -b preview/embassy_union_rebase origin/feat/embassy-mem-channel-cap
cd /tmp/embassy_rebase_preview
git rebase --onto <TARGET> 1a4ca83
```

`<TARGET>` = `origin/feature/pr1_124_followups` (pre-spine-merge) or `origin/feature/phase21_api_symmetry` (post-merge). Rebasing from `1a4ca83` drops the three duplicates by construction.

- [ ] **Step 2: Resolve `63e549f` (socket_manager.rs)**

One hunk: HEAD's `use crate::log::{…}` vs embassy's `use super::CLIENT_SOCKET_CHANNEL_CAP;` at the same insertion point. Keep both (CLIENT_SOCKET_CHANNEL_CAP line first, then the log line). `git add` + `git rebase --continue`.

- [ ] **Step 3: Resolve `d56a691` (runtime.rs)**

One hunk: embassy inserts `async fn accept_subscribe<T, Sub>(…)` (a ~95-line function) above `handle_sd_message`; HEAD's side of the hunk is only the doc line `/// Handle a Service Discovery message (Subscribe / \`FindService\` etc.).`. Resolution: take the entire embassy insertion, and replace its trailing un-backticked `FindService` doc line with the backticked HEAD version. `git add` + continue.

- [ ] **Step 4: Resolve `77cf725` (3 files, 5 hunks)**

Every hunk is the union contract (HEAD) vs the 4-param contract (embassy) of the SAME semantic content — the union strictly supersedes (it has everything plus `ctx` + `source`). Keep the HEAD side of **every** hunk; do NOT use whole-file `checkout --ours` (it would discard the commit's cleanly-merged additions elsewhere in those files). `git add -u` + continue.

- [ ] **Step 5: Confirm completion**

`3f7d7d1` and `223bf40` apply clean. Expected: `Successfully rebased`. Then `cargo +nightly check --no-default-features --features bare-metal-runtime,client 2>&1 | grep -c "^error"` → exactly 2 (both at `runtime.rs:345`) — that's the Task 3–5 worklist, not a failure.

---

### Task 3: Union adoption — `DispatchFn`, `DISPATCH_CTX`, trampoline

**Files:**
- Modify: `src/bare_metal_runtime/runtime.rs` (~line 63 alias; ~line 174 statics; ~line 263 trampoline; ~line 345 registration; ~line 441 init store; the init config struct — locate fields with `grep -n "pub dispatch\|dispatch:" src/bare_metal_runtime/runtime.rs`)

- [ ] **Step 1: Reshape the alias** (~line 63)

```rust
/// Platform dispatch sink for inbound messages (decoded by the runtime).
/// Same shape as [`crate::server::NonSdRequestCallback`] — the union
/// contract — so a platform can use one handler for both the server's
/// non-SD observer and the runtime's notification RX path. `ctx` is the
/// opaque word registered at [`init`]; `source` is the sender;
/// `e2e_status` is real on the RX path (Profile-5 check) and `0`
/// (unchecked) on the server-request path.
pub type DispatchFn = fn(
    ctx: usize,
    source: core::net::SocketAddrV4,
    service_id: u16,
    method_id: u16,
    payload: &[u8],
    e2e_status: u8,
);
```

- [ ] **Step 2: Add the parallel ctx static** (next to `static DISPATCH`, ~line 174)

```rust
static DISPATCH: AtomicUsize = AtomicUsize::new(0); // DispatchFn as usize
static DISPATCH_CTX: AtomicUsize = AtomicUsize::new(0); // opaque ctx word for DISPATCH
```

- [ ] **Step 3: Register ctx at init**

The init config struct (the one whose fields feed `SEND_FN`/`NOW_FN`/`DISPATCH` stores at ~line 439-441) gains a field:

```rust
    /// Opaque context word passed back verbatim as the first argument of
    /// every `dispatch` invocation (FFI: stash a pointer as `usize`).
    pub dispatch_ctx: usize,
```

and beside `DISPATCH.store(...)` (~line 441):

```rust
    DISPATCH_CTX.store(config.dispatch_ctx, Ordering::Release);
```

**Flag for Feliciano:** if that struct is `#[repr(C)]` consumed from C, this is a C-side ABI addition — field at the END of the struct, and the C header updates with it.

- [ ] **Step 4: Reshape the trampoline** (~line 263)

```rust
/// Forwards a parsed inbound message to the platform dispatch callback.
/// The `_ctx` received from the caller is ignored: the runtime's real
/// ctx lives in [`DISPATCH_CTX`] (registered at [`init`], possibly
/// re-registered later), so loading it here keeps late re-registration
/// coherent — callers register/pass `0`.
fn dispatch(
    _ctx: usize,
    source: core::net::SocketAddrV4,
    service_id: u16,
    method_id: u16,
    payload: &[u8],
    e2e_status: u8,
) {
    let raw = DISPATCH.load(Ordering::Acquire);
    if raw == 0 {
        return;
    }
    // SAFETY: stored from a valid DispatchFn in `init`.
    let f: DispatchFn = unsafe { core::mem::transmute::<usize, DispatchFn>(raw) };
    f(
        DISPATCH_CTX.load(Ordering::Acquire),
        source,
        service_id,
        method_id,
        payload,
        e2e_status,
    );
}
```

- [ ] **Step 5: Fix the registration** (~line 345)

```rust
            non_sd_observer: Some((dispatch as crate::server::NonSdRequestCallback, 0)),
```

(`0` because the trampoline injects `DISPATCH_CTX` itself — see Step 4 doc.)

- [ ] **Step 6: Verify the two errors are gone**

Run: `cargo +nightly check --no-default-features --features bare-metal-runtime,client 2>&1 | grep -E "^error" | head`
Expected: errors at `runtime.rs:345` gone; remaining errors (if any) are in `bare_metal_tasks.rs` — Task 4's worklist.

---

### Task 4: Thread ctx + source through `event_rx_dispatch_future`

**Files:**
- Modify: `src/bare_metal_tasks.rs` (~lines 95-125, plus its callers — locate with `grep -n "event_rx_dispatch_future" src/`)

- [ ] **Step 1: Reshape the helper**

The fn's `dispatch` parameter is currently an inline 4-param fn type. It becomes the union shape plus a pass-through `ctx`, and the receive captures the datagram's source (`ReceivedDatagram.source` is already there — only `bytes_received` was being kept):

```rust
pub async fn event_rx_dispatch_future<'a, S, R>(
    rx_socket: &'a S,
    e2e: &'a R,
    e2e_enabled: bool,
    dispatch: crate::bare_metal_runtime::DispatchFn,
    ctx: usize,
    buf: &'a mut [u8],
) where
    S: TransportSocket,
    R: E2ERegistryHandle,
{
    loop {
        let (n, source) = match rx_socket.recv_from(&mut *buf).await {
            Ok(d) => (d.bytes_received, d.source),
            Err(_) => continue,
        };
        let Some(parsed) = parse_someip_datagram(&buf[..n]) else {
            continue;
        };
        let (status, body) = if e2e_enabled {
            check_parsed_e2e(e2e, &parsed)
        } else {
            (E2ECheckStatus::Unchecked, parsed.payload)
        };
        dispatch(
            ctx,
            source,
            parsed.service_id,
            parsed.method_id,
            body,
            e2e_status_code(status),
        );
    }
}
```

NOTE on the `dispatch` param type: if `bare_metal_tasks` must stay decoupled from the `bare-metal-runtime` feature (check the cfg on `bare_metal_runtime`'s module declaration in lib.rs), keep an inline fn type with the same six params instead of naming `DispatchFn`. External (non-runtime) callers pass their real callback + ctx and get verbatim forwarding; the runtime passes its trampoline + `0`.

- [ ] **Step 2: Update the callers**

`run_someip` (in `bare_metal_tasks.rs`) and any direct caller pass the extra `ctx` argument — the runtime's composition passes `0` (trampoline injects). Locate: `grep -n "event_rx_dispatch_future(" src/`.

- [ ] **Step 3: Sweep for leftover 4-param shapes**

Run: `grep -rn "fn(service_id: u16, method_id: u16" src/`
Expected: zero hits (every dispatch-shaped type is now the union).

- [ ] **Step 4: Full check**

Run: `cargo +nightly check --no-default-features --features bare-metal-runtime,client 2>&1 | tail -2` → `Finished`.

- [ ] **Step 5: Commit** (one commit on the preview branch for Tasks 3+4)

```bash
git add src/bare_metal_runtime/runtime.rs src/bare_metal_tasks.rs
git commit -m "feat(bare-metal): DispatchFn adopts the union callback contract

Same six-param shape as NonSdRequestCallback (decision 2026-06-11):
ctx + source + decoded fields + e2e_status. DISPATCH_CTX parallel
static carries the opaque word; the dispatch trampoline injects it so
late re-registration stays coherent; event_rx_dispatch_future threads
ctx/source through (e2e_status stays REAL on this path).

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: CHANGELOG + sd_codec visibility note

**Files:**
- Modify: `CHANGELOG.md`

- [ ] **Step 1: Behavior notes under the 0.8.0 section**

Append to the existing `#### Breaking — NonSdRequestCallback…` block's vicinity (match file style):

- `PENDING_RESPONSES_CAP` 64→8 is a **global** bound (also tokio): more than 8 outstanding request-response pairs now returns `Err(Error::Capacity(…))`. Sized for the embedded target; raise the const if a host consumer genuinely needs more in flight. (`REQUEST_QUEUE_CAP` 32→4 is NOT consumer-visible: the feeding control channel was always depth 4 on both paths — verified `BoundedSender<ControlMessage, 4>` + tokio `channel(N)`.)
- The bare-metal runtime's `DispatchFn` now matches `NonSdRequestCallback` (union shape); the init config gains `dispatch_ctx`.

- [ ] **Step 2: sd_codec visibility — decision recorded, not changed**

`sd_codec::parse_someip_datagram` stays at its current visibility (the runtime's RX path uses it; halo's FFI may too). Narrowing to `pub(crate)` is Feliciano's call on his own PR — leave a PR-comment question, don't change it here.

- [ ] **Step 3: Commit**

```bash
git add CHANGELOG.md
git commit -m "docs: changelog for ARENA cap bounds + DispatchFn union shape

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: Full verification

- [ ] **Step 1: fmt the conflict resolutions**

Run: `cargo fmt` then `git diff --stat` — the Task 2 resolutions (esp. `accept_subscribe`) may need reflow; if fmt changed files, amend them into the rebase HEAD: `git add -u && git commit --amend --no-edit` is WRONG here (HEAD is the Task 5 commit) — instead commit fmt separately: `git commit -m "style: rustfmt over rebase resolutions"`.

- [ ] **Step 2: The matrix**

```bash
cargo fmt --check
cargo clippy --workspace --all-features -- -D warnings -D clippy::pedantic
cargo clippy --no-default-features -- -D warnings -D clippy::pedantic
cargo test --features server-tokio,client-tokio,bare_metal --tests --lib -- --test-threads=1
cargo test --doc
cargo check -p simple-someip-embassy-net --tests
cargo +nightly build --no-default-features --features client,bare_metal -Zbuild-std=core --target thumbv7em-none-eabihf
cargo +nightly build --no-default-features --features server,bare_metal -Zbuild-std=core --target thumbv7em-none-eabihf
cargo +nightly build --no-default-features --features client,server,bare_metal -Zbuild-std=core --target thumbv7em-none-eabihf
cargo +nightly build --no-default-features --features bare-metal-runtime,client -Zbuild-std=core --target thumbv7em-none-eabihf
cargo clean -p simple-someip --target thumbv7em-none-eabihf
cargo build --target thumbv7em-none-eabihf --no-default-features --features server,bare_metal
nm -A target/thumbv7em-none-eabihf/debug/libsimple_someip.rlib | grep -c -E '__rust_alloc|__rg_alloc'
```

Expected: all clean; nm prints `0`. The fourth build-std line is NEW (the runtime feature under halo's constraint) — if it fails on `embassy-executor` deps, record the failure verbatim; it's a finding about #128, not about this rebase. The future-size witnesses run inside the test step and must PASS (the cap cuts shrink futures; budgets are upper bounds). If the `#128` clippy surface has pre-existing pedantic warnings the chain's gate now catches (the chain enforces `--workspace --all-features`), fix mechanically and commit as `style:`.

- [ ] **Step 3: Probe-mirror check (`7623829` touched payload/option sizes)**

`tools/size_probe`'s `ProbePayload` mirrors `TestPayload` (`src/protocol/sd/test_support.rs`) field-for-field. `7623829` changed `heapless_payload.rs` / `sd/options.rs` / `static_channels` — verify `TestPayload`/`TestSdHeader` themselves are untouched (`git diff 1a4ca83..HEAD -- src/protocol/sd/test_support.rs` → empty means the mirror holds). `MAX_CONFIGURATION_STRING_LENGTH` changes WILL shift captured layouts — that's expected and handled by Step 4's re-capture, not an error.

- [ ] **Step 4: Fresh baseline — this is PR 2's "before"**

```bash
tools/capture_type_sizes.sh
```

Copy the new numbers into a new committed baseline `docs/simple_someip/plans/baselines/post-128-size-baseline.md` (same format as `pr0-size-baseline.md`, with a header noting: captured on the #128-rebased tree; supersedes pr0 baseline as PR 2's "before"; the deltas vs pr0 quantify #128's cap cuts — record them, they're the first real measured win of the stack). Also re-run the host witnesses with `--nocapture` and record the `FUTURE_SIZE` lines.

```bash
git add docs/simple_someip/plans/baselines/post-128-size-baseline.md
git commit -m "docs: post-#128 size baseline (PR 2's before; quantifies the cap cuts)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 7: Witness-budget tightening decision (optional, record either way)

The PR 0 witness budgets are `pr0-baseline × 1.25`. After #128's cuts the real sizes drop well below those budgets, leaving slack that could mask a future regression up to the OLD budget. Either tighten the budget consts (`src/client/mod.rs`, `tests/bare_metal_e2e.rs`) to `post-128-baseline × 1.25` in this branch, or record in the new baseline doc that tightening lands with PR 2. **Default: tighten now** — it's two consts per file and the witnesses exist to be tight.

---

### Task 8: Handoff (Feliciano coordination — DO NOT force-push his branch unilaterally)

- [ ] **Step 1: Push the preview**

```bash
git push -u origin preview/embassy_union_rebase
```

- [ ] **Step 2: PR comment on #128**

Summarize: rebase preview ready; 3 duplicate commits dropped; his 9 commits survive with authorship intact (rebase preserves author); union contract adopted for `DispatchFn` + `DISPATCH_CTX` + init-config `dispatch_ctx` field (C-side ABI addition flagged); the conflict inventory + this plan's path; ask him to either `git reset --hard origin/preview/embassy_union_rebase && git push --force-with-lease` on his branch, or cherry-pick at his leisure. Include the open question: `sd_codec` visibility (keep `pub` for halo FFI, or narrow?).

- [ ] **Step 3: Sequencing reminder**

This branch can only MERGE after the spine (#124→#127→#129) lands in phase21 — it contains the spine. If #129 gets amended in review, re-run Task 1 Step 1's check and rebase the preview again (cheap: the inventory holds).

---

## Self-review notes (already applied)

- The dry run IS the spec-coverage check: every #128 commit is accounted for in the inventory table; the 2 compile errors are closed by Tasks 3–4; sweep step (Task 4 Step 3) catches any 4-param stragglers.
- `e2e_status` semantics differ by path and both docs say so: REAL on the RX/notification path (`check_parsed_e2e`), `0` on the server-request path — the union docs on both aliases carry the distinction.
- The trampoline-injects-ctx design (register `(dispatch, 0)`) was chosen over registering the real ctx because `DISPATCH`/`DISPATCH_CTX` support late re-registration; the server's stored copy would go stale. Documented on the trampoline.
- `event_rx_dispatch_future` gets ctx as a pass-through parameter (not a static read) because it's a public spawnable helper — non-runtime callers need verbatim forwarding.
