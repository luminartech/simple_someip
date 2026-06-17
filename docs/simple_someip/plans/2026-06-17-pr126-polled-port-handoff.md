# Handoff: port #126 (polled bare-metal) onto the #125 stack

**For:** Feliciano (owner of the polled module)
**Branch:** `feat/polled-port-onto-125` — branched off the #133 tip (`82be01d`), the top of the completed #125 stack (`#131 → #127 → #129 → #132 → #133`). This is your port target; it builds clean.
**Why a port, not a rebase:** `#126`'s `polled.rs` is built on your *own* divergent refactor of the alloc-free server, the `E2ERegistry`, and a new mutex — and the #125 stack reworked those same areas differently. A `git rebase` of `#126` onto #133 hits 15 conflict hunks across `server/mod.rs`/`runtime.rs`/`subscription_manager.rs` and replays commits that duplicate (and contradict) the stack. So the clean path is: branch off #133, bring `polled.rs` over, and adapt it to the stack's APIs.

## Decision already made (Justin, 2026-06-17): take the union callback

`NonSdRequestCallback` is **already the agreed union** on the stack — no change needed there. At `src/server/mod.rs:666`:
```rust
pub type NonSdRequestCallback = fn(
    ctx: usize, source: core::net::SocketAddrV4,
    service_id: u16, method_id: u16, payload: &[u8], e2e_status: u8,
);
```
Library-side header parse happens at the call site (`runtime.rs:619`, `view.header()`) — the consumer never hand-rolls parsing (MISRA/ASIL). `ctx: usize` (not `*mut c_void`) keeps `Server: Send`; `source` keeps reply-routing; `e2e_status` is live on the client path. halo uses neither `ctx` nor `source`, but they stay for other consumers. **Your `#126` commit `4c2531d "remove non-SD observer callback"` is therefore dropped — we keep the observer.** Apply this same shape to the runtime `DispatchFn` when you bring it over.

## Your `#126` commits — keep / drop / adapt

| commit | what | action |
|---|---|---|
| `4faac4a` make ...alloc-free, add `NonSdRequestCallback` | server/mod, runtime, subscription_manager | **DROP** — superseded by `#131` (stack's alloc-free server) |
| `4c2531d` remove non-SD observer | server/mod (−28), runtime (−13) | **DROP** — superseded by the union decision |
| `929962a` const-generic `E2ERegistry` | e2e/registry, subscription_manager (+164), transport, event_publisher | **DON'T adopt into the stack** — see "E2E" below; adapt polled instead |
| `372c7d4` `SingleContextRawMutex` | new file, subscription_manager, transport | **your call** — bring the primitive if polled needs it (see "mutex") |
| `25ba82b` sync SOME/IP helpers | new `src/polled.rs` (+285), header.rs | **KEEP** — port |
| `3b8e483` polled integration | `polled.rs` (+464) | **KEEP** — port |
| `e7c956c` multi-offer builders | `polled.rs` (+114) | **KEEP** — port |

Bring the polled sources over with:
```bash
git checkout feat/polled-port-onto-125
git checkout origin/feat/polled-bared-metal -- src/polled.rs
# (and src/single_context_mutex.rs if you decide to keep the mutex)
# then wire `mod polled;` into src/lib.rs and adapt — see below.
```

## Target API the stack provides (what to adapt `polled.rs` against)

- **`NonSdRequestCallback`** — the union above (`src/server/mod.rs:666`); registered via `ServerDeps`/`ServerStorage.non_sd_observer: Option<(NonSdRequestCallback, usize)>` and invoked at `runtime.rs:619`.
- **`E2ERegistry`** (`src/e2e/registry.rs`) is a **plain, non-const-generic** struct: `register(key, profile) -> Result<(), E2ERegistryFull>`, `contains_key(&key) -> bool`, `check(...)`, `protect(...)`. The handle trait `E2ERegistryHandle` and `StaticE2EHandle` live in `src/transport.rs` as the stack left them (PR 2/PR 3). **Your `929962a` changed these to a const-generic (`E2E_CAP`) shape** — `polled.rs` (`check_parsed_e2e<const E2E_CAP>`, the `E2E_CAP` params) and its `use crate::transport::E2ERegistryHandle` / `use crate::StaticE2EHandle` depend on that. Adapt polled's E2E usage to the stack's non-const-generic handle surface.
- **Server send/run** (PR 3 — caller-buffer model): `Server::run_with_buffers(unicast_buf, sd_buf, recv_send_buf, announce_send_buf: &mut [u8])`; new public `Server::announce_only_with_buffer(&mut [u8])`; `EventPublisher::publish_event_with_buffers(.., msg_buf, protected_buf)` / `publish_raw_event_with_buffers(.., buf)`. The SD send helpers in `runtime.rs`/`sd_state.rs` now take caller scratch and bound on `buf.len()`. If polled re-implements SD datagram building (`build_multi_offer_service_datagram`), reconcile against these.
- **`subscription_manager`** caps: `EVENT_GROUPS_CAP = 32`, `SUBSCRIBERS_PER_GROUP = 16`; backed by `FnvIndexMap`. `929962a` reshaped this (+164) — adapt polled's `<const EG, const SUBS>` usage to the stack's.

## Adaptation list (the divergent-API touch points in `polled.rs`)

1. **E2E:** `use crate::transport::E2ERegistryHandle`, `use crate::StaticE2EHandle`, `use crate::E2ECheckStatus`, and `check_parsed_e2e<const E2E_CAP>` (`polled.rs:341`) — reconcile with the stack's non-const-generic `E2ERegistry`/handle traits. **Do NOT pull `929962a` into the stack** to satisfy this (see rationale below); adapt polled.
2. **Mutex:** any `SingleContextRawMutex` use — decide whether to bring `372c7d4`'s primitive (it's small and arguably generally useful) or use the stack's existing mutex abstraction.
3. **Server/observer:** polled's references to the removed observer / your alloc-free server APIs — retarget to the stack's union `NonSdRequestCallback` + `#131`/PR 3 server surface.
4. **SD builders:** `build_multi_offer_service_datagram` / `build_multi_stop_offer_service_datagram` (`polled.rs:145/158`) — confirm they encode against the same SD wire format the stack's `sd_state`/`runtime` helpers use (wire format is unchanged across the #125 stack).

## Why the const-generic `E2ERegistry` (`929962a`) is NOT being adopted into the stack

Adopting it would re-open the audited E2E code that PR 2/PR 3 just reworked and reviewed (the E2E-OOB fixes + `buf.len()` guards), force a public `E2ERegistry` API migration on other consumers (e.g. dft, which uses the E2E/`e2e_status` surface), and add per-instantiation monomorphization/flash cost the flash-constrained TC4/halo target doesn't want — all to host one module. If the const-generic registry is worth it on its own merits, it should be its own PR with its own review + dft-impact call, not a polled dependency. So: `polled.rs` (a consumer) adapts to the crate's reviewed E2E surface.

## Suggested order

1. `git checkout origin/feat/polled-bared-metal -- src/polled.rs` onto this branch; wire `mod polled;` into `lib.rs`.
2. Compile under `--no-default-features --features <your polled feature set>`; fix the E2E-handle + mutex + observer references against the target APIs above.
3. Re-apply the union `NonSdRequestCallback` shape to the runtime `DispatchFn`.
4. Run `cargo build --target thumbv7em-none-eabihf ...` for the polled feature combos + the no-alloc witness.
5. Clear the open `#126` review punch list (mutex feature-unification footgun, missing tests/CI, Ack-on-failure) as part of this.
6. Open the PR based on `feature/pr3_125_server_buffers` (keep it stacked; never merge-down).

`#128` (embassy-mem-channel-cap) is still a draft and stacks *after* this once it lands.

Backups of the original tips: `backup/feat-polled-bared-metal-20260617-prestack2`, `backup/feat-embassy-mem-channel-cap-20260617-prestack2`.
