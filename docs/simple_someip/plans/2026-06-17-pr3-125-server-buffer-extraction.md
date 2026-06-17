# PR 3 — #125 Server Buffer Extraction + Final Numbers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move the server's 6 future-resident `[u8; UDP_BUFFER_SIZE]` send-path buffers out of the run/publish futures into caller-provided scratch — matching the server's existing caller-provided *receive* buffer model (`run_with_buffers`) — then capture issue #125's final client+server before/after numbers and close it.

**Architecture:** The server runs a single combined future (`run_combined` = `recv_loop` + `announce_loop` via `select`) plus an app-driven publish path (`EventPublisher`); it does NOT spawn per-socket loops, so there is no buffer *pool* — buffers are caller-provided fixed scratch (as the receive buffers already are). The SD/Subscribe/offer send helpers and `EventPublisher::publish_*` stop stack-allocating `[u8; UDP_BUFFER_SIZE]` and instead take `&mut [u8]` scratch; the run path is fed two scratch buffers (recv-path + announce-path, which can be mid-send concurrently); the tokio path heap-allocates them internally so the public tokio API is unchanged.

**Tech Stack:** Rust (edition 2024, crate stays stable-buildable), `heapless`, `embassy-sync`, `tokio` (std path only), cargo, `cargo-nextest`.

**Spec:** `docs/simple_someip/plans/2026-06-09-phase22-125-memory-reduction-design.md` (PR 3 section — note its `recv_loop`/`announce_loop` *receive* extraction is already done via `run_with_buffers`; the real targets are the send paths below).

**Base:** stacked on PR 2 (`feature/pr2_125_client_async_state`, tip `6b0aaa3`).

## Global Constraints

- **Crate stays stable-buildable**; no nightly-only features. **`server,bare_metal` stays alloc-free** where it is today (the new scratch params must not pull `alloc` into the bare-metal path — tokio-only heap allocation goes behind the `_alloc`/`server-tokio` gate).
- **Wire format untouched.** No change to any byte emitted.
- **Public tokio/std API unchanged** for `Server::new`/`run` and `EventPublisher::publish_*` callers: the tokio path allocates the new scratch internally (mirroring how `run_inner` already heap-allocates the 65535-byte receive buffers).
- **Only behavioral change allowed:** an encode or E2E-protect output that exceeds the *provided scratch length* is rejected with `Error::Capacity("udp_buffer")` (today it's checked against the `UDP_BUFFER_SIZE` constant, which is correct only because the buffers are currently full-size). No other behavioral change.
- **Existing suite green on every commit** (514 nextest `client-tokio,server-tokio`; `bare_metal_e2e` 6/6; no-alloc witness; all clippy + doc gates).
- **Every memory change validated against the `bm_server_run_future` witness** (`tests/bare_metal_e2e.rs`); a change that doesn't move the number is dropped.
- **Exact constants (verbatim):** `UDP_BUFFER_SIZE = 1500` (`src/lib.rs:158`); `BM_SERVER_RUN_FUTURE_BUDGET = 9664` (measured 7696; `tests/bare_metal_e2e.rs:606`).

---

## File Structure

| File | Action | Responsibility |
|---|---|---|
| `src/server/runtime.rs` | Modify | `send_unicast_offer`/`send_subscribe_ack_from_view`/`send_subscribe_nack_from_view` take `buf: &mut [u8]`; `recv_loop`/`announce_loop`/`run_combined` thread the two send-scratch buffers to them. |
| `src/server/sd_state.rs` | Modify | `SdStateManager::send_offer_service` takes `buf: &mut [u8]`; bounds on `buf.len()`. |
| `src/server/event_publisher.rs` | Modify | `publish_event` takes `msg_buf`/`protected_buf: &mut [u8]`; `publish_raw_event` takes `buf: &mut [u8]`; all encode + E2E bounds on `buf.len()`. |
| `src/server/mod.rs` | Modify | Extend `run_with_buffers` with the two send buffers; `run_inner` (tokio) heap-allocates them; tokio `EventPublisher` wrapper allocates publish scratch internally. |
| `tests/bare_metal_e2e.rs` | Modify | Server send-path regression tests (small-scratch → `Capacity`, not panic) + retighten `BM_SERVER_RUN_FUTURE_BUDGET`. |

---

### Task 1: SD send helpers take caller scratch (`runtime.rs`)

**Files:** Modify `src/server/runtime.rs` (`send_unicast_offer` ~`:29-78`, `send_subscribe_ack_from_view` ~`:81-129`, `send_subscribe_nack_from_view` ~`:132-182`); Test `tests/bare_metal_e2e.rs`.

**Interfaces:**
- Produces: each helper gains a leading `buf: &mut [u8]` parameter (replacing its internal `let mut buffer = [0u8; UDP_BUFFER_SIZE]`).

- [ ] **Step 1: Write the failing test** (a too-small scratch rejects, doesn't panic/OOB)

```rust
// tests/bare_metal_e2e.rs — drives send_subscribe_ack via the public Subscribe path
// with a deliberately tiny send-scratch buffer; the encoded SD-ACK exceeds it.
#[tokio::test]
async fn server_send_with_undersized_scratch_returns_capacity_not_panic() {
    // Harness: build the server with a send-scratch buffer of, say, 24 bytes —
    // big enough for the 16-byte header but not the SD-ACK payload — drive a
    // Subscribe, and assert the run loop surfaces Capacity("udp_buffer") and
    // does NOT panic / OOB. (Model on the existing bare_metal_e2e server harness.)
    let outcome = run_server_subscribe_with_send_scratch_len(24).await;
    assert!(matches!(outcome, Err(Error::Capacity("udp_buffer"))));
}
```

- [ ] **Step 2: Run it — expect FAIL** (helpers don't take a buffer yet / no harness)

Run: `cargo test --features client,server,bare_metal --test bare_metal_e2e server_send_with_undersized_scratch`
Expected: FAIL (signature mismatch / harness missing).

- [ ] **Step 3: Change the three helpers to take `buf` and bound on `buf.len()`**

Pattern (apply to all three — `send_unicast_offer`, `send_subscribe_ack_from_view`, `send_subscribe_nack_from_view`):

```rust
// was: fn send_subscribe_ack_from_view(... sd_socket, ...) { let mut buffer = [0u8; UDP_BUFFER_SIZE]; ... }
async fn send_subscribe_ack_from_view(
    buf: &mut [u8],               // ← caller scratch (was the local array)
    /* existing params... */
) -> Result<(), Error> {
    if buf.len() < 16 {
        return Err(Error::Capacity("udp_buffer"));
    }
    let sd_data_len = sd_payload.encode_to_slice(&mut buf[16..])?; // encode_to_slice already errors if the slice is too small; map/propagate to Capacity if it surfaces a different error
    let someip_header = SomeIpHeader::new_sd(sid, sd_data_len);
    someip_header.encode_to_slice(&mut buf[..16])?;
    let total_len = 16 + sd_data_len;
    if total_len > buf.len() {                       // defensive: should already be caught by encode_to_slice
        return Err(Error::Capacity("udp_buffer"));
    }
    sd_socket.send_to(&buf[..total_len], subscriber_v4).await?;
    Ok(())
}
```
Note: `encode_to_slice` into `&mut buf[16..]` already fails on a too-small slice — verify which error it returns and ensure the helper surfaces `Error::Capacity("udp_buffer")` for the over-capacity case (add the explicit `buf.len()` guards above so the contract is the typed capacity error, not a generic encode error).

- [ ] **Step 4: Run the test — expect PASS**, then the full server suite.

Run: `cargo test --features client,server,bare_metal --test bare_metal_e2e server_send_with_undersized_scratch` → PASS
Run: `cargo nextest run --no-default-features --features client-tokio,server-tokio` → still green.

- [ ] **Step 5: Commit**

```bash
git add src/server/runtime.rs tests/bare_metal_e2e.rs
git commit -m "feat(server): SD send helpers take caller scratch, bound on buf.len() (#125)"
```

---

### Task 2: `SdStateManager::send_offer_service` takes caller scratch (`sd_state.rs`)

**Files:** Modify `src/server/sd_state.rs:~219-240`.

**Interfaces:** Consumes nothing new. Produces: `send_offer_service` gains a `buf: &mut [u8]` parameter.

- [ ] **Step 1: Change the signature and bound on `buf.len()`** (same pattern as Task 1; this is the `announce_loop`'s send path):

```rust
// was: pub(crate) async fn send_offer_service(&self, config, socket) { let mut buffer = [0u8; UDP_BUFFER_SIZE]; ... }
pub(crate) async fn send_offer_service(
    &self,
    buf: &mut [u8],          // ← caller scratch
    config: &ServerConfig,
    socket: &impl TransportSocket,
) -> Result<(), Error> {
    if buf.len() < 16 { return Err(Error::Capacity("udp_buffer")); }
    let sd_data_len = sd_payload.encode_to_slice(&mut buf[16..])?;
    let someip_header = SomeIpHeader::new_sd(sid, sd_data_len);
    someip_header.encode_to_slice(&mut buf[..16])?;
    let total_len = 16 + sd_data_len;
    if total_len > buf.len() { return Err(Error::Capacity("udp_buffer")); }
    let multicast_addr = SocketAddrV4::new(sd::MULTICAST_IP, sd::MULTICAST_PORT);
    socket.send_to(&buf[..total_len], multicast_addr).await?;
    Ok(())
}
```

- [ ] **Step 2: Build (callers updated in Task 3) — verify it compiles in isolation by temporarily building after Task 3 wires the caller.** (This task's deliverable is verified together with Task 3, since `send_offer_service`'s only caller is `announce_loop`.)

- [ ] **Step 3: Commit** (fold into Task 3's commit if cleaner — `send_offer_service` has no standalone caller).

```bash
git add src/server/sd_state.rs
git commit -m "feat(server): send_offer_service takes caller scratch, bound on buf.len() (#125)"
```

---

### Task 3: Thread two send-scratch buffers through the run path (`runtime.rs`, `server/mod.rs`)

**Files:** Modify `src/server/runtime.rs` (`run_combined` ~`:587-642`, `recv_loop` ~`:435-571`, `announce_loop` ~`:397-430`); `src/server/mod.rs` (`run_with_buffers` ~`:1260-1312`, `run_inner` ~`:1403-1453`).

**Interfaces:**
- Consumes: the `buf`-taking helpers (Tasks 1–2).
- Produces: `Server::run_with_buffers(unicast_buf, sd_buf, recv_send_buf, announce_send_buf: &mut [u8])` — two new send-scratch params. `recv_loop` and `announce_loop` each receive their send-scratch buffer.

**Why two:** `run_combined` drives `recv_loop` and `announce_loop` concurrently via `select`; both can be suspended at a `send_to().await` at the same time, so a single shared send buffer would alias. `recv_loop` itself handles one inbound message at a time (one send in flight), so it needs exactly one; `announce_loop` needs exactly one.

- [ ] **Step 1: Extend `run_with_buffers` + `run_combined` + the loops to pass the buffers down**

```rust
// server/mod.rs — run_with_buffers gains two params
pub fn run_with_buffers<'a>(
    &self,
    unicast_buf: &'a mut [u8],
    sd_buf: &'a mut [u8],
    recv_send_buf: &'a mut [u8],      // ← new: recv_loop's send scratch
    announce_send_buf: &'a mut [u8],  // ← new: announce_loop's send scratch
) -> impl core::future::Future<Output = Result<(), Error>> + 'a + use<'a, F, Tm, R, Sub, H, Hsd, Hep> { /* forward all four into run_combined */ }
```
```rust
// runtime.rs — run_combined forwards recv_send_buf into recv_loop, announce_send_buf into announce_loop;
// recv_loop passes its buffer to send_unicast_offer / send_subscribe_ack_from_view / send_subscribe_nack_from_view;
// announce_loop passes announce_send_buf to sd_state.send_offer_service.
```

- [ ] **Step 2: tokio `run_inner` allocates the two send buffers internally** (API unchanged for `Server::run` callers):

```rust
// server/mod.rs run_inner (tokio) — alongside the existing two recv vecs
let mut unicast_buf = alloc::vec![0u8; 65535];
let mut sd_buf = alloc::vec![0u8; 65535];
let mut recv_send_buf = alloc::vec![0u8; crate::UDP_BUFFER_SIZE];     // ← new
let mut announce_send_buf = alloc::vec![0u8; crate::UDP_BUFFER_SIZE]; // ← new
// ...run_with_buffers(&mut unicast_buf, &mut sd_buf, &mut recv_send_buf, &mut announce_send_buf).await
```

- [ ] **Step 3: Update bare-metal callers** (`examples/bare_metal_server`, any `run_with_buffers` test caller) to declare and pass the two send buffers. Grep for `run_with_buffers(` and fix each call site.

- [ ] **Step 4: Verify** — full suite + bare-metal builds:

Run: `cargo nextest run --no-default-features --features client-tokio,server-tokio` → green
Run: `cargo build --target thumbv7em-none-eabihf --no-default-features --features server,bare_metal` → builds; `cargo build ... --features client,server,bare_metal` → builds
Run: `cargo test --features client,server,bare_metal --test bare_metal_e2e` → 6/6 + Task 1's new test

- [ ] **Step 5: Commit**

```bash
git add src/server/runtime.rs src/server/sd_state.rs src/server/mod.rs examples/ tests/
git commit -m "feat(server): thread recv+announce send-scratch buffers through run_with_buffers (#125)"
```

---

### Task 4: `EventPublisher` publish paths take caller scratch (`event_publisher.rs`, `server/mod.rs`)

**Files:** Modify `src/server/event_publisher.rs` (`publish_event` ~`:158-218`, `publish_raw_event` ~`:313-344`); `src/server/mod.rs` (tokio publisher wrapper / publish entry).

**Interfaces:**
- Produces: `publish_event(&self, /* msg */, msg_buf: &mut [u8], protected_buf: &mut [u8])` and `publish_raw_event(&self, /* hdr+payload */, buf: &mut [u8])`. E2E needs `protected_buf` because `E2ERegistry::protect(key, input: &[u8], hdr, output: &mut [u8])` requires disjoint in/out slices.

- [ ] **Step 1: Write the failing test** (E2E publish on undersized scratch → `Capacity`, mirroring PR 2's client regression):

```rust
// tests/bare_metal_e2e.rs — register an E2E key, publish an event whose protected
// frame exceeds the provided msg_buf/protected_buf, assert Capacity not panic/OOB.
#[tokio::test]
async fn e2e_publish_with_undersized_scratch_returns_capacity_not_panic() {
    let outcome = publish_e2e_event_with_scratch_len(40).await; // 36 fits, +12 P4 protect = 48 > 40
    assert!(matches!(outcome, Err(Error::Capacity("udp_buffer"))));
}
```

- [ ] **Step 2: Run it — expect FAIL.**

- [ ] **Step 3: Change `publish_event` / `publish_raw_event` to take scratch and bound on `buf.len()`** (the E2E guard is the PR-2 lesson applied here — `> msg_buf.len()` / `> protected_buf.len()`, not `> UDP_BUFFER_SIZE`):

```rust
// publish_event: was two stack arrays; now caller scratch.
pub async fn publish_event(&self, /* message */, msg_buf: &mut [u8], protected_buf: &mut [u8]) -> Result<(), Error> {
    let mut message_length = message.encode_to_slice(msg_buf)?;     // errors if msg_buf too small
    if self.e2e_registry.contains_key(&key) {
        let upper_header: [u8; 8] = msg_buf[8..16].try_into().expect("upper header slice");
        let result = self.e2e_registry.protect(key, &msg_buf[16..message_length], upper_header, protected_buf);
        if let Some(Ok(protected_len)) = result {
            if 16 + protected_len > msg_buf.len() {                  // ← buf.len(), not UDP_BUFFER_SIZE
                return Err(Error::Capacity("udp_buffer"));
            }
            msg_buf[16..16 + protected_len].copy_from_slice(&protected_buf[..protected_len]);
            message_length = 16 + protected_len;
        } /* preserve the existing Some(Err)/None arms */
    }
    let datagram = &msg_buf[..message_length];
    for addr in &subscribers { /* existing send_to(datagram).await loop, unchanged */ }
    Ok(())
}
```

- [ ] **Step 4: tokio publisher keeps the app API ergonomic** — the `server-tokio` publish wrapper allocates `msg_buf`/`protected_buf` (Vecs) internally per publish so existing `publish_event(message)` callers are unchanged; only the bare-metal publish path surfaces the `&mut [u8]` params. Gate the internal allocation behind `_alloc`/`server-tokio`.

- [ ] **Step 5: Run the test (PASS) + full suite + no-alloc witness.**

Run: `cargo test --features client,server,bare_metal --test bare_metal_e2e e2e_publish_with_undersized_scratch` → PASS
Run: `cargo nextest run --no-default-features --features client-tokio,server-tokio` → green
Run: `cargo test --features client,bare_metal --test no_alloc_witness` → still alloc-free (publish scratch alloc is tokio-gated)

- [ ] **Step 6: Commit**

```bash
git add src/server/event_publisher.rs src/server/mod.rs tests/bare_metal_e2e.rs
git commit -m "feat(server): EventPublisher publish paths take caller scratch; E2E bound on buf.len() (#125)"
```

---

### Task 5: Measure + retighten the server run-future witness

**Files:** Modify `tests/bare_metal_e2e.rs:606`.

- [ ] **Step 1: Capture before/after**

Run: `cargo test --features client,server,bare_metal --test bare_metal_e2e -- --nocapture | grep bm_server_run_future`
Record the new `bm_server_run_future` (expected to drop from 7696 as the recv-path + announce-path send buffers leave `run_combined`'s state).

- [ ] **Step 2: Set the budget to `ceil64(new × 1.25)`**

```rust
// tests/bare_metal_e2e.rs:606
const BM_SERVER_RUN_FUTURE_BUDGET: usize = /* ceil64(new bm_server_run_future × 1.25) */;
```

- [ ] **Step 3: Verify** the witness passes at the tightened budget. If the number did NOT move, the run path's buffers weren't the dominant term — record that and keep the extraction only if it helps (per the binding constraint).

- [ ] **Step 4: Commit**

```bash
git add tests/bare_metal_e2e.rs
git commit -m "test(server): retighten server run-future budget after send-buffer extraction (#125)"
```

---

### Task 6: Final #125 numbers + close-out

**Files:** none (verification + PR description); optionally `CHANGELOG.md`.

- [ ] **Step 1: Full gate matrix**

```bash
cargo nextest run --no-default-features --features client-tokio,server-tokio
cargo test --features client,server,bare_metal --test bare_metal_e2e
cargo test --features client,bare_metal --test no_alloc_witness
cargo clippy --workspace --all-features -- -D warnings -D clippy::pedantic
cargo clippy --no-default-features -- -D warnings -D clippy::pedantic
cargo build --target thumbv7em-none-eabihf --no-default-features --features server,bare_metal
cargo build --target thumbv7em-none-eabihf --no-default-features --features client,server,bare_metal
# nm: server,bare_metal alloc status is documented (server uses alloc today); client,bare_metal stays alloc-free
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
cargo test --doc --all-features
```

- [ ] **Step 2: Capture authoritative thumb numbers** (if `tools/capture_type_sizes.sh` exists) and assemble issue #125's acceptance table: client (PR 2: socket-loop 2224→776) + server (PR 3: `bm_server_run_future` before→after) TaskStorage sizes + any pool/`nm` symbol deltas.

- [ ] **Step 3: Record the before/after in the PR description**; note in CHANGELOG that the server run/publish buffers are now caller-sized (breaking the `run_with_buffers` and `publish_*` bare-metal signatures — free pre-0.8.0).

- [ ] **Step 4: Finish the branch** — announce and use superpowers:finishing-a-development-branch; push `feature/pr3_125_server_buffers` and open the draft PR **based on `feature/pr2_125_client_async_state`** (keep it stacked; never merge-down).

---

## Self-Review

**Spec coverage:** all 6 future-resident send buffers → Tasks 1 (3 SD helpers), 2 (offer), 4 (publish ×2 incl. E2E `protected`). Threading → Task 3 (run) + Task 4 (publish). E2E `buf.len()` bound → Task 4. Witness retighten → Task 5. Final #125 tables → Task 6. The receive buffers are already caller-provided (no task). ✓

**Placeholder scan:** the witness budget (Task 5) and the test harness shims (`run_server_subscribe_with_send_scratch_len`, `publish_e2e_event_with_scratch_len`) are resolved at implementation against the real `bare_metal_e2e` harness; the `encode_to_slice` error-mapping in Tasks 1–2 must be matched to the real return type. Flagged inline.

**Type consistency:** `buf: &mut [u8]` is the uniform scratch param across Tasks 1/2/4; `run_with_buffers` gains exactly `recv_send_buf`/`announce_send_buf` (Task 3) consumed by the Task 1/2 helpers; `publish_event` gains `msg_buf`/`protected_buf`, `publish_raw_event` gains `buf` (Task 4). `Error::Capacity("udp_buffer")` reused verbatim.

**Design note for the reviewer:** unlike the client (a `BufferProvider` *pool* for dynamically-spawned per-socket loops), the server uses fixed caller-provided scratch because it runs one combined future + an app publish path — there is no dynamic socket spawning, so claim/release is unnecessary. This is the deliberate, architecture-driven divergence from the client.
