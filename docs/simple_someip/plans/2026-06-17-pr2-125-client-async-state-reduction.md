# PR 2 — #125 Client Async-State Reduction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move the client's per-socket `[u8; UDP_BUFFER_SIZE]` buffers out of the spawned socket-loop futures and into caller-sized pooled storage, and flatten the control-message handler, so the client's Embassy-arena (TaskStorage) footprint drops and becomes consumer-sized — closing the client half of issue #125.

**Architecture:** Three moves. (1) Add a `BufferPool` static-storage primitive and a `BufferProvider` trait that mirror the existing channel-pool machinery (`OneshotPool`/`MpscPool` + `ChannelFactory`); a claim returns a RAII `BufferLease` that derefs to `&'static mut [u8]` and returns its slot on drop. (2) Thread a `BufferProvider` through `ClientDeps` → `BindDispatch` → `SocketManager::bind_*`; the socket loop receives its buffer by slice instead of owning a stack array, so the buffer lives in consumer `.bss` (or the tokio heap pool), not in the future. (3) Flatten `handle_control_message` into a synchronous decode/decide returning a small action value, with awaits hoisted to shallow helpers — kept only where the PR-0 witnesses show it moves the number.

**Tech Stack:** Rust (edition 2024, crate stays stable-buildable), `heapless`, `embassy-sync` (bare-metal channel backend), `critical-section` (no_std slot synchronization), `tokio` (std path only), cargo, `cargo-nextest`.

**Spec:** `docs/simple_someip/plans/2026-06-09-phase22-125-memory-reduction-design.md` (PR 2 section) and its rev-2 caller-sized-buffers decision.

## Global Constraints

- **Crate stays stable-buildable.** No nightly-only features in `simple-someip` itself. (CI may use nightly only for `-Zprint-type-sizes` / `-Zbuild-std` measurement jobs.)
- **Wire format untouched.** No change to any byte emitted or parsed.
- **Public tokio/std API unchanged.** Callers using the tokio-defaulted `ClientDeps` constructor see no signature change; the buffer pool is provisioned internally (8 × `UDP_BUFFER_SIZE`).
- **Only these behavioral changes are allowed** (all from the design doc's PR-2 section; everything else is behavior-preserving):
  - (a) An inbound datagram larger than the claimed receive buffer is **dropped with a log**, not truncated/panicked.
  - (b) The oversize-send rejection compares against the **claimed buffer length** (`buf.len()`), not the `UDP_BUFFER_SIZE` constant.
- **Existing suite stays green on every commit** (~543 tests as of #114 plus #124's additions; the `client,bare_metal` no-alloc witness and the embassy-net loopback live-wire test included).
- **Every memory change is validated against PR-0's future-size witnesses** (`tests/bare_metal_e2e.rs`). A change that does not move a witness number is dropped, not merged on faith.
- **No `&'static mut` aliasing.** A buffer slot is handed out to exactly one lease at a time; the pool enforces this and is covered by a witness test.
- **Exact constants (copy verbatim):** `UDP_BUFFER_SIZE = 1500` (`src/lib.rs:158`); `UNICAST_SOCKETS_CAP = 8` (`src/client/inner.rs:40`).

---

## File Structure

| File | Action | Responsibility |
|---|---|---|
| `src/static_channels/buffer_pool.rs` | Create | `BufferPool<SLOTS, LEN>` static primitive + `BufferLease` RAII handle (claim/release, no aliasing). |
| `src/static_channels/mod.rs` | Modify | `mod buffer_pool; pub use buffer_pool::{BufferPool, BufferLease};` |
| `src/transport.rs` | Modify | `BufferProvider` trait (`claim(&self) -> Option<BufferLease>`), next to the `*Pooled` traits. |
| `src/client/socket_manager.rs` | Modify | `socket_loop_future` takes the buffer by lease; oversize-send check keys off `buf.len()`; inbound-oversize drop+log; `bind_*` claim a buffer before spawn. |
| `src/client/bind_dispatch.rs` | Modify | Thread the `BufferProvider` from `SpawnerDispatch` into the `SocketManager::bind_*` calls. |
| `src/client/inner.rs` | Modify | Hold the provider; flatten `handle_control_message` into a sync decide + hoisted awaits. |
| `src/client/mod.rs` | Modify | Add `buffer_provider` field/generic to `ClientDeps`; rewrite the `:1-30` memory-footprint doc. |
| `src/tokio_transport.rs` | Modify | Heap-backed `BufferProvider` (leak a `[u8; UDP_BUFFER_SIZE] × 8` store) wired into the tokio-defaulted `ClientDeps` constructor. |
| `tests/buffer_pool.rs` | Create | Unit + witness tests for claim-to-exhaustion, release-on-drop, no double-claim. |
| `tests/bare_metal_e2e.rs` | Modify | Extend/retighten the future-size witnesses after extraction. |

---

### Task 1: `BufferPool` static primitive + `BufferLease`

**Files:**
- Create: `src/static_channels/buffer_pool.rs`
- Modify: `src/static_channels/mod.rs`
- Test: `tests/buffer_pool.rs`

**Interfaces:**
- Produces:
  - `pub struct BufferPool<const SLOTS: usize, const LEN: usize>` with `pub const fn new() -> Self` and `pub fn claim(&'static self) -> Option<BufferLease>`.
  - `pub struct BufferLease { /* private */ }` implementing `Deref<Target=[u8]>`, `DerefMut`, `Drop` (returns the slot), and `Send`.

- [ ] **Step 1: Write the failing test**

```rust
// tests/buffer_pool.rs
use simple_someip::static_channels::BufferPool;

static POOL: BufferPool<2, 4> = BufferPool::new();

#[test]
fn claim_returns_distinct_zeroed_slices_until_exhausted() {
    let mut a = POOL.claim().expect("slot 0");
    let b = POOL.claim().expect("slot 1");
    assert_eq!(a.len(), 4);
    assert_eq!(&*b, &[0u8; 4]);            // freshly handed-out slot is zeroed
    a[0] = 0xAB;                            // writable
    assert_eq!(a[0], 0xAB);
    assert!(POOL.claim().is_none(), "pool of 2 must refuse a 3rd claim");
}

#[test]
fn dropping_a_lease_returns_its_slot() {
    let a = POOL.claim().expect("slot");
    drop(a);
    assert!(POOL.claim().is_some(), "slot must be reusable after the lease drops");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test buffer_pool`
Expected: FAIL — `BufferPool` is not found / unresolved import.

- [ ] **Step 3: Write the implementation**

```rust
// src/static_channels/buffer_pool.rs
//! Fixed-capacity pool of `&'static mut [u8]` buffers with claim/release
//! semantics, mirroring the channel pools in this module. A `BufferPool`
//! is declared as a `static` by the consumer; each `claim()` hands out one
//! slot as a [`BufferLease`] that returns the slot to the pool on drop.
//!
//! Synchronization uses `critical-section` so the same code is valid on the
//! bare-metal (single-core, no atomics-guarantee) target and on std.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};

/// Backing storage for a pool: `SLOTS` independent `LEN`-byte buffers plus a
/// claimed-flag per slot.
pub struct BufferPool<const SLOTS: usize, const LEN: usize> {
    // `UnsafeCell` because `claim()` hands out `&'static mut` into this store;
    // the `claimed` flags guarantee at most one live `&mut` per slot.
    store: UnsafeCell<[[u8; LEN]; SLOTS]>,
    claimed: UnsafeCell<[bool; SLOTS]>,
}

// SAFETY: all access to `store`/`claimed` is funneled through a
// `critical_section::with`, which provides mutual exclusion on the targets
// we support; a slot is only aliased once (its `claimed` flag gates it).
unsafe impl<const SLOTS: usize, const LEN: usize> Sync for BufferPool<SLOTS, LEN> {}

impl<const SLOTS: usize, const LEN: usize> BufferPool<SLOTS, LEN> {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            store: UnsafeCell::new([[0u8; LEN]; SLOTS]),
            claimed: UnsafeCell::new([false; SLOTS]),
        }
    }

    /// Claim a free slot, or `None` if all `SLOTS` are in use. The returned
    /// buffer is zeroed before hand-out so a reused slot never leaks the
    /// previous tenant's bytes.
    pub fn claim(&'static self) -> Option<BufferLease> {
        critical_section::with(|_| {
            // SAFETY: inside the critical section we hold exclusive access to
            // both arrays; we take a raw pointer and only form one `&mut` for
            // the chosen, not-yet-claimed slot.
            let claimed = unsafe { &mut *self.claimed.get() };
            let idx = claimed.iter().position(|&c| !c)?;
            claimed[idx] = true;
            let store = unsafe { &mut *self.store.get() };
            let slot: &'static mut [u8; LEN] = unsafe { &mut *(&mut store[idx] as *mut [u8; LEN]) };
            slot.fill(0);
            Some(BufferLease {
                buf: slot.as_mut_slice(),
                claimed_flag: claimed.as_mut_ptr(),
                idx,
            })
        })
    }
}

impl<const SLOTS: usize, const LEN: usize> Default for BufferPool<SLOTS, LEN> {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII handle to one claimed buffer. Derefs to the `&'static mut [u8]`;
/// returns the slot to its pool on drop.
pub struct BufferLease {
    buf: &'static mut [u8],
    claimed_flag: *mut bool,
    idx: usize,
}

// SAFETY: `BufferLease` owns exclusive access to its slot (gated by the
// pool's `claimed` flag) and the backing store is `'static`.
unsafe impl Send for BufferLease {}

impl Deref for BufferLease {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.buf
    }
}

impl DerefMut for BufferLease {
    fn deref_mut(&mut self) -> &mut [u8] {
        self.buf
    }
}

impl Drop for BufferLease {
    fn drop(&mut self) {
        critical_section::with(|_| {
            // SAFETY: `claimed_flag` points into the owning pool's `'static`
            // `claimed` array; only this lease writes `idx`'s flag.
            unsafe {
                *self.claimed_flag.add(self.idx) = false;
            }
        });
    }
}
```

```rust
// src/static_channels/mod.rs  — add near the other `mod`/`pub use` lines
mod buffer_pool;
pub use buffer_pool::{BufferLease, BufferPool};
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --test buffer_pool`
Expected: PASS (both tests).

- [ ] **Step 5: Confirm no_std-clean and lint-clean**

Run: `cargo clippy -p simple-someip --no-default-features --features client,bare_metal -- -D warnings -D clippy::pedantic`
Expected: no warnings. (`critical-section` is already a transitive dep via `embassy-sync` per `Cargo.toml`; if the bare-metal build cannot find it, add `critical-section = { version = "1", optional = true }` and include it in the `bare_metal` feature.)

- [ ] **Step 6: Commit**

```bash
git add src/static_channels/buffer_pool.rs src/static_channels/mod.rs tests/buffer_pool.rs
git commit -m "feat(static_channels): BufferPool + BufferLease claim/release primitive (#125)"
```

---

### Task 2: `BufferProvider` trait + static & tokio impls

**Files:**
- Modify: `src/transport.rs` (next to `OneshotPooled`/`BoundedPooled`, ~`:1342`)
- Modify: `src/tokio_transport.rs` (after the `TokioChannels` `*Pooled` impls, ~`:550`)
- Test: `tests/buffer_pool.rs`

**Interfaces:**
- Consumes: `BufferLease`, `BufferPool` (Task 1).
- Produces:
  - `pub trait BufferProvider: Clone + Send + Sync + 'static { fn claim(&self) -> Option<BufferLease>; }`
  - `pub struct StaticBufferProvider<const SLOTS: usize, const LEN: usize>(pub &'static BufferPool<SLOTS, LEN>);` impl `BufferProvider`.
  - `pub struct TokioBufferProvider;` impl `BufferProvider` (heap-backed, `UDP_BUFFER_SIZE`-sized).

- [ ] **Step 1: Write the failing test**

```rust
// tests/buffer_pool.rs  (append)
use simple_someip::static_channels::{BufferPool, BufferLease};
use simple_someip::transport::{BufferProvider, StaticBufferProvider};

static PROV_POOL: BufferPool<2, 8> = BufferPool::new();

#[test]
fn static_provider_claims_through_a_shared_pool() {
    let prov = StaticBufferProvider(&PROV_POOL);
    let _a = prov.claim().expect("first");
    let _b = prov.claim().expect("second");
    assert!(prov.claim().is_none(), "provider exposes the pool's capacity");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test buffer_pool static_provider_claims_through_a_shared_pool`
Expected: FAIL — `BufferProvider` / `StaticBufferProvider` unresolved.

- [ ] **Step 3: Write the trait and static impl**

```rust
// src/transport.rs  — near the *Pooled traits
use crate::static_channels::{BufferLease, BufferPool};

/// Source of `&'static mut [u8]` receive/scratch buffers for the client's
/// socket loops. Mirrors [`ChannelFactory`]'s role for channels: the
/// bare-metal path is backed by a consumer-declared `static BufferPool`;
/// the tokio path is heap-backed and provisioned internally.
pub trait BufferProvider: Clone + Send + Sync + 'static {
    /// Claim one buffer, or `None` when the pool is exhausted.
    fn claim(&self) -> Option<BufferLease>;
}

/// `BufferProvider` backed by a `'static` [`BufferPool`] (bare-metal path).
#[derive(Clone, Copy, Debug)]
pub struct StaticBufferProvider<const SLOTS: usize, const LEN: usize>(
    pub &'static BufferPool<SLOTS, LEN>,
);

impl<const SLOTS: usize, const LEN: usize> BufferProvider
    for StaticBufferProvider<SLOTS, LEN>
{
    fn claim(&self) -> Option<BufferLease> {
        self.0.claim()
    }
}
```

```rust
// src/tokio_transport.rs  — heap-backed provider
use crate::static_channels::{BufferLease, BufferPool};
use crate::transport::BufferProvider;
use crate::UDP_BUFFER_SIZE;

/// Tokio-path buffer provider: a single leaked `BufferPool` sized at
/// `UNICAST_SOCKETS_CAP + 1` × `UDP_BUFFER_SIZE` (one per possible socket
/// plus discovery). Leaking is fine — a client process holds one for its
/// lifetime; the API hides it entirely from callers.
#[derive(Clone, Copy, Debug)]
pub struct TokioBufferProvider(&'static BufferPool<9, UDP_BUFFER_SIZE>);

impl TokioBufferProvider {
    #[must_use]
    pub fn new() -> Self {
        Self(Box::leak(Box::new(BufferPool::new())))
    }
}

impl Default for TokioBufferProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl BufferProvider for TokioBufferProvider {
    fn claim(&self) -> Option<BufferLease> {
        self.0.claim()
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --test buffer_pool static_provider_claims_through_a_shared_pool`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/transport.rs src/tokio_transport.rs tests/buffer_pool.rs
git commit -m "feat(transport): BufferProvider trait + static/tokio impls (#125)"
```

---

### Task 3: `socket_loop_future` consumes the buffer; oversize behaviors

**Files:**
- Modify: `src/client/socket_manager.rs:551` (signature), `:569` (drop local `buf`), `:447-458` (oversize-send check), the receive path (inbound-oversize drop+log)
- Test: `tests/bare_metal_e2e.rs` (a focused inbound-oversize test)

**Interfaces:**
- Consumes: `BufferLease` (Task 1).
- Produces: `socket_loop_future<T, R>(socket, rx_tx, tx_rx, e2e_registry, buf: BufferLease)` — the buffer is now an explicit parameter.

- [ ] **Step 1: Write the failing test** (inbound datagram larger than the claimed buffer is dropped with a log, loop survives)

```rust
// tests/bare_metal_e2e.rs  (new test; reuse the file's existing harness types)
#[tokio::test]
async fn inbound_datagram_larger_than_claimed_buffer_is_dropped_not_fatal() {
    // Claim a deliberately tiny 8-byte buffer for the loop, deliver a 64-byte
    // datagram, then deliver a valid small one. The oversized datagram must be
    // dropped (no panic, loop still running) and the valid one delivered.
    let outcome = run_socket_loop_with_buffer_len(8, &[
        Datagram::raw(vec![0xFF; 64]),       // oversized -> dropped
        Datagram::valid_small(),             // must still arrive
    ])
    .await;
    assert_eq!(outcome.delivered.len(), 1, "only the in-budget datagram is delivered");
    assert!(outcome.loop_alive, "loop must survive an oversized datagram");
}
```

*(If `run_socket_loop_with_buffer_len` / `Datagram` helpers don't exist, add a thin harness in this test module that builds a `SocketManager` over a mock `TransportSocket` whose `recv` yields the scripted datagrams and a `BufferPool<1, 8>` for the lease. Model it on the existing `future_size_witness_bare_metal_channels` setup at `tests/bare_metal_e2e.rs:600`.)*

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --features client,server,bare_metal --test bare_metal_e2e inbound_datagram_larger`
Expected: FAIL — signature mismatch (`socket_loop_future` takes no buffer) / helper missing.

- [ ] **Step 3: Change the signature and drop the local array**

```rust
// src/client/socket_manager.rs:551 — add the buffer parameter
#[allow(clippy::too_many_lines)]
async fn socket_loop_future<T, R>(
    socket: T,
    rx_tx: C::BoundedSender<Result<ReceivedMessage<MessageDefinitions>, Error>, 16>,
    mut tx_rx: C::BoundedReceiver<SendMessage<MessageDefinitions, C>, 16>,
    e2e_registry: R,
    mut buf: crate::static_channels::BufferLease,   // ← was `let mut buf = [0u8; UDP_BUFFER_SIZE];`
) where
    T: TransportSocket + 'static,
    R: E2ERegistryHandle,
{
    const MAX_CONSECUTIVE_RECV_ERRORS: u32 = 16;
    let mut consecutive_recv_errors: u32 = 0;
    // (delete the old `let mut buf = [0u8; UDP_BUFFER_SIZE];` at :569)
```

- [ ] **Step 4: Inbound-oversize drop+log in the receive path**

In the receive arm (where `socket.recv(&mut buf)` is awaited), the transport already truncates to `buf.len()`; add the guard that a datagram reported larger than `buf.len()` is dropped with a log rather than parsed from a truncated buffer:

```rust
// receive path, after a successful recv reporting `n` bytes:
let n = match socket.recv(&mut buf).await {
    Ok(n) => n,
    Err(e) => { /* existing consecutive-error handling, unchanged */ }
};
if n > buf.len() {
    crate::log::warn!(
        "inbound datagram ({n} B) exceeds claimed buffer ({} B); dropping",
        buf.len()
    );
    continue;
}
let datagram = &buf[..n];
// ... existing parse/forward of `datagram`, unchanged
```

- [ ] **Step 5: Oversize-send check keys off `buf.len()`**

```rust
// src/client/socket_manager.rs:447 — was `if required > UDP_BUFFER_SIZE {`
if required > buf.len() {
    warn!(
        "outgoing message size {required} exceeds claimed buffer ({}); rejecting with Capacity(\"udp_buffer\")",
        buf.len()
    );
    return Err(Error::Capacity("udp_buffer"));
}
```

For the E2E `protected` scratch at `:632` (`let mut protected = [0u8; UDP_BUFFER_SIZE];`): leave it for **Task 5** (measurement-gated). For now, keep it as a local so this task stays focused on the receive buffer + the two oversize behaviors.

- [ ] **Step 6: Run the focused test + the full client/server suite**

Run: `cargo test --features client,server,bare_metal --test bare_metal_e2e inbound_datagram_larger`
Expected: PASS.
Run: `cargo nextest run --no-default-features --features client-tokio,server-tokio`
Expected: all existing client/server tests still green (behavior-preserving except the two allowed changes).

- [ ] **Step 7: Commit**

```bash
git add src/client/socket_manager.rs tests/bare_metal_e2e.rs
git commit -m "feat(client): socket loop receives buffer by lease; oversize drop/reject on buf.len() (#125)"
```

---

### Task 4: Thread `BufferProvider` through deps → bind → spawn

**Files:**
- Modify: `src/client/mod.rs:278-296` (`ClientDeps` gains a `buffer_provider` + generic `BP`), and the tokio-defaulted constructor
- Modify: `src/client/bind_dispatch.rs:34-117` (`BindDispatch` + `SpawnerDispatch` carry/forward the provider)
- Modify: `src/client/socket_manager.rs:248-249,355-397` (claim a buffer before spawn; pass it into `socket_loop_future`)
- Modify: `src/client/inner.rs` (store the provider; nothing extra at eviction — release is RAII on loop exit)
- Test: `tests/bare_metal_e2e.rs` (bind-to-capacity claims/releases)

**Interfaces:**
- Consumes: `BufferProvider` (Task 2), `socket_loop_future(.., buf)` (Task 3).
- Produces: `ClientDeps<F, Tm, R, I, Sp, BP: BufferProvider>` with field `pub buffer_provider: BP`.

- [ ] **Step 1: Write the failing test** (binding N unicast sockets claims N buffers; closing a socket releases its buffer)

```rust
// tests/bare_metal_e2e.rs
#[tokio::test]
async fn each_bound_socket_claims_one_buffer_and_releases_on_close() {
    // Pool with exactly 2 slots; provider shared into ClientDeps.
    static POOL: simple_someip::static_channels::BufferPool<2, 1500> =
        simple_someip::static_channels::BufferPool::new();
    let provider = simple_someip::transport::StaticBufferProvider(&POOL);

    let client = build_test_client_with_buffer_provider(provider).await;
    client.bind_unicast_for_test(40000).await.expect("1st bind claims slot 0");
    client.bind_unicast_for_test(40001).await.expect("2nd bind claims slot 1");
    // 3rd bind must fail: pool exhausted.
    let third = client.bind_unicast_for_test(40002).await;
    assert!(matches!(third, Err(Error::Capacity("udp_buffer"))));

    client.close_unicast_for_test(40000).await;          // releases slot 0
    client.bind_unicast_for_test(40002).await.expect("slot freed -> bind succeeds");
}
```

*(`build_test_client_with_buffer_provider` / `bind_unicast_for_test` / `close_unicast_for_test` are thin test shims over `new_with_deps` + the existing control-message API; build them in the test module mirroring `tests/bare_metal_client.rs:257`.)*

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --features client,server,bare_metal --test bare_metal_e2e each_bound_socket_claims`
Expected: FAIL — `ClientDeps` has no `buffer_provider`.

- [ ] **Step 3: Add the provider to `ClientDeps`**

```rust
// src/client/mod.rs:278 — add generic BP and field
pub struct ClientDeps<F, Tm, R, I, Sp, BP>
where
    F: TransportFactory,
    Tm: Timer,
    R: E2ERegistryHandle,
    I: InterfaceHandle,
    BP: crate::transport::BufferProvider,
{
    pub factory: F,
    pub timer: Tm,
    pub e2e_registry: R,
    pub interface: I,
    pub spawner: Sp,
    /// Source of `&'static mut [u8]` socket-loop buffers (caller-sized on
    /// bare-metal; internally heap-provisioned on the tokio path).
    pub buffer_provider: BP,
}
```

Update `Client::new_with_deps` and the `Inner` constructor to store `buffer_provider` (alongside `dispatch`/`spawner`). The tokio-defaulted constructor (the phase-21 `ClientDeps`/`Deps` convenience builder) sets `buffer_provider: TokioBufferProvider::new()` so tokio callers are unaffected.

- [ ] **Step 4: Forward the provider through `bind_dispatch` and claim at bind**

In `SpawnerDispatch<F, S>` add a `buffer_provider: BP` field; in its `BindDispatch::bind_unicast`/`bind_discovery` impls (`src/client/bind_dispatch.rs:87-117`), claim a buffer and pass it to the `SocketManager::bind_*` calls. In `SocketManager::bind_with_transport` / `bind_discovery_seeded_with_transport` (`socket_manager.rs:355-397` / `:248`), accept the lease and forward it into the spawned loop:

```rust
// src/client/bind_dispatch.rs — bind_unicast impl
fn bind_unicast(&self, port: u16, e2e_registry: R) -> impl Future<Output = Result<SocketManager<MD, C>, Error>> + '_ {
    async move {
        let buf = self
            .buffer_provider
            .claim()
            .ok_or(Error::Capacity("udp_buffer"))?;   // pool exhausted -> typed error
        SocketManager::<MD, C>::bind_with_transport(
            &self.factory,
            &self.spawner,
            port,
            e2e_registry,
            buf,                                        // ← moved into the loop
        )
        .await
    }
}
```

```rust
// src/client/socket_manager.rs:389 — pass buf into the spawned future
let fut = Self::socket_loop_future(socket, rx_tx, tx_rx, e2e_registry, buf);
spawner.spawn(fut);
```

Release needs **no** new code: the lease is owned by the loop future, so when a socket closes and the loop returns, the future drops, dropping the lease and freeing the slot — the eviction at `inner.rs:639` already removes the handle. (Add a one-line comment there noting the buffer is released via the loop future's drop.)

- [ ] **Step 5: Run the test + the no-alloc witness**

Run: `cargo test --features client,server,bare_metal --test bare_metal_e2e each_bound_socket_claims`
Expected: PASS.
Run: `cargo test --features client,bare_metal --test no_alloc_witness`
Expected: PASS — the buffer pool introduces no allocator symbols on the client path.

- [ ] **Step 6: Commit**

```bash
git add src/client/mod.rs src/client/bind_dispatch.rs src/client/socket_manager.rs src/client/inner.rs tests/bare_metal_e2e.rs
git commit -m "feat(client): thread BufferProvider through deps/bind; release on loop drop (#125)"
```

---

### Task 5: E2E scratch buffer — measurement-gated

**Files:**
- Modify: `src/client/socket_manager.rs:629-632` (the `protected` E2E buffer)
- Reference: `tests/bare_metal_e2e.rs` witnesses

**Interfaces:** consumes the per-loop receive `BufferLease` (Task 3/4).

The design doc leaves this as measure-and-decide. Two candidate implementations; pick by the witness numbers.

- [ ] **Step 1: Capture the current `bm_client_socket_loop` witness number**

Run: `cargo test --features client,server,bare_metal --test bare_metal_e2e future_size_witness_bare_metal_channels -- --nocapture | grep FUTURE_SIZE`
Record the `bm_client_socket_loop` value (post-Task-4 baseline).

- [ ] **Step 2: Implement Option A — reuse the loop's receive buffer for E2E protect**

The receive buffer and the E2E send-scratch are never live at the same instant (receive and send are distinct `select` arms processed one at a time). Reuse the single leased `buf` for protection instead of a second 1500-byte array:

```rust
// src/client/socket_manager.rs:629 — was `let mut protected = [0u8; UDP_BUFFER_SIZE];`
// Reuse the loop's leased buffer; nothing inbound is pending across a send.
let protected = &mut buf[..];
let protected_len = e2e_registry.protect(&key, &outgoing, protected)?; // adjust to actual protect() sig
socket.send_to(&protected[..protected_len], dest).await?;
```

If `protect()` requires the input and output to be disjoint slices (it may, depending on its signature), fall back to **Option B**: claim a second `BufferLease` from the same provider at send time (`provider.claim()`), use it for `protected`, and let it drop at the end of the send arm. Decide A-vs-B by which keeps `bm_client_socket_loop` smaller in the witness.

- [ ] **Step 3: Re-measure and verify the suite**

Run: `cargo test --features client,server,bare_metal --test bare_metal_e2e -- --nocapture | grep FUTURE_SIZE`
Expected: `bm_client_socket_loop` ≤ the Step-1 number (strictly smaller if the second array was the dominant term).
Run: `cargo nextest run --no-default-features --features client-tokio,server-tokio`
Expected: green, including any E2E send test.

- [ ] **Step 4: Commit**

```bash
git add src/client/socket_manager.rs
git commit -m "perf(client): eliminate the second E2E scratch buffer from the socket loop (#125)"
```

---

### Task 6: Flatten `handle_control_message` (secondary, keep only if it moves the number)

**Files:**
- Modify: `src/client/inner.rs:661-970` (`handle_control_message`), `:1034-1235` (`run_future`)
- Test: existing control-message tests + the run-future witness

**Interfaces:** introduces a private `enum ControlAction` describing the post-decode work; awaits are hoisted to `run_future`'s top level.

- [ ] **Step 1: Capture the current `bm_client_run_future` witness number**

Run: `cargo test --features client,server,bare_metal --test bare_metal_e2e future_size_witness_bare_metal_channels -- --nocapture | grep bm_client_run_future`
Record the value.

- [ ] **Step 2: Add the action enum and split decode from await**

Split the 10-variant `match` (`inner.rs:661-970`) into (i) a synchronous `decide_control_action(&mut self, msg) -> ControlAction` that does all the lock/registry mutation and returns a small value, and (ii) shallow `async` helpers invoked from `run_future` for the arms that must await (`bind_*`, `SendToService`, `SendSD`, `Subscribe`, `QueryRebootFlag`):

```rust
// src/client/inner.rs — new private enum
enum ControlAction {
    None,
    BindDiscovery(C::OneshotSender<Result<(), Error>>),
    BindUnicastThenSend { service_id: u16, instance_id: u16, message: /*…*/, /* + responders */ },
    SendSd { target: SocketAddrV4, header: SdHeader, response: /*…*/ },
    Subscribe { /* the Subscribe fields */ },
    QueryRebootFlag(C::OneshotSender<Result<RebootFlag, Error>>),
    // …one variant per arm that currently awaits
}
```

```rust
// run_future loop tail (was `self.handle_control_message().await;` at :1234)
let action = self.decide_control_action();      // synchronous; drops all locals before awaiting
match action {
    ControlAction::None => {}
    ControlAction::BindDiscovery(resp) => {
        let r = self.bind_discovery().await;     // shallow, single await
        let _ = resp.send(r);
    }
    ControlAction::BindUnicastThenSend { .. } => { /* hoisted await */ }
    // …
}
```

The awaited sub-futures still appear in `run_future`'s layout; the win is that the per-variant locals (decoded headers, buffers, responder handles) are no longer held across the awaits, and the variants overlap better. **This task is kept only if Step 4 shows the witness moved.**

- [ ] **Step 3: Run the full control-message suite (behavior must be identical)**

Run: `cargo nextest run --no-default-features --features client-tokio,server-tokio`
Expected: all green — every control path (bind/unbind, send, subscribe, reboot-flag query, set-interface) behaves exactly as before.

- [ ] **Step 4: Re-measure the run-future witness**

Run: `cargo test --features client,server,bare_metal --test bare_metal_e2e -- --nocapture | grep bm_client_run_future`
Expected: value ≤ Step-1. **If it did not drop, revert this task** (`git checkout -- src/client/inner.rs`) per the "dropped, not merged on faith" constraint, and note it in the PR description.

- [ ] **Step 5: Commit (only if kept)**

```bash
git add src/client/inner.rs
git commit -m "perf(client): flatten control-message handler to shrink run_future state (#125)"
```

---

### Task 7: Tighten witness budgets + rewrite the footprint doc

**Files:**
- Modify: `tests/bare_metal_e2e.rs:596-598` (budgets)
- Modify: `src/client/mod.rs:1-30` (doc)

- [ ] **Step 1: Set budgets to the new sizes + 25% headroom**

Using the post-Task-6 `FUTURE_SIZE` prints, set each budget to `ceil64(measured × 1.25)`:

```rust
// tests/bare_metal_e2e.rs:596 — replace with the new measured baselines
const BM_CLIENT_RUN_FUTURE_BUDGET: usize = /* ceil64(new bm_client_run_future × 1.25) */;
const BM_CLIENT_SOCKET_LOOP_BUDGET: usize = /* ceil64(new bm_client_socket_loop × 1.25) */;
const BM_SERVER_RUN_FUTURE_BUDGET: usize = 9664; // unchanged — server is PR 3
```

- [ ] **Step 2: Rewrite the memory-footprint doc** (`src/client/mod.rs:1-30`) to describe pooled, caller-sized buffers instead of the old "12 KiB always-live / 24 KiB peak in-future" math:

```rust
//! SOME/IP client.
//!
//! # Memory footprint
//!
//! The client's `Inner` state is allocated inline. The per-socket
//! `UDP_BUFFER_SIZE` receive buffers are **not** part of the spawned
//! socket-loop futures: each loop claims a `&'static mut [u8]` from a
//! [`BufferProvider`] at bind and releases it when the socket closes. On
//! the bare-metal path the consumer declares the backing `BufferPool` as a
//! `static`, choosing both the slot count and the per-slot length (e.g.
//! 2 × 512 B), so the buffer budget lives in `.bss` and is sized by the
//! caller rather than fixed at `UNICAST_SOCKETS_CAP × UDP_BUFFER_SIZE`. On
//! `std + tokio` the provider is heap-backed and provisioned internally
//! (`UDP_BUFFER_SIZE`-sized slots), invisible to callers.
//!
//! See `docs/simple_someip/plans/2026-06-09-phase22-125-memory-reduction-design.md`.
```

- [ ] **Step 3: Verify witnesses pass at the new budgets + docs build**

Run: `cargo test --features client,server,bare_metal --test bare_metal_e2e`
Expected: PASS at the tightened budgets.
Run: `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --no-default-features --features client`
Expected: no broken intra-doc links (the `[`BufferProvider`]` link resolves).

- [ ] **Step 4: Commit**

```bash
git add tests/bare_metal_e2e.rs src/client/mod.rs
git commit -m "docs+test(client): pooled-buffer footprint doc + tightened future-size budgets (#125)"
```

---

### Task 8: Full verification + before/after numbers

**Files:** none (verification + PR description)

- [ ] **Step 1: Full suite, all shipped feature combos**

```bash
cargo nextest run --no-default-features --features client-tokio,server-tokio
cargo test --features client,bare_metal --test no_alloc_witness
cargo clippy --workspace --all-features -- -D warnings -D clippy::pedantic
cargo clippy -p simple-someip --no-default-features --features client,bare_metal -- -D warnings -D clippy::pedantic
cargo build --target thumbv7em-none-eabihf --no-default-features --features client,bare_metal
```
Expected: all green; client+bare_metal still alloc-free.

- [ ] **Step 2: Capture authoritative thumb numbers** (if `tools/capture_type_sizes.sh` from PR 0 exists)

Run: `tools/capture_type_sizes.sh`
Record the client run-future / socket-loop TaskStorage rows.

- [ ] **Step 3: Record before/after** in the PR description — the PR-0 baseline vs. the post-PR-2 `FUTURE_SIZE` prints and thumb table, calling out the arena bytes moved to caller `.bss`.

- [ ] **Step 4: Finish the branch**

Announce: "I'm using the finishing-a-development-branch skill to complete this work." Then follow superpowers:finishing-a-development-branch — verify the suite, push `feature/pr2_125_client_async_state`, and open the draft PR **based on `feature/pr1_124_followups`** (keep it stacked, never merge-down).

---

## Self-Review

**Spec coverage (design doc PR-2 section):**
- Buffer extraction via claim/release pool, `&'static mut [u8]`, caller-sized → Tasks 1, 2, 3, 4. ✓
- Tokio path provisions internally, API unchanged → Task 2 (`TokioBufferProvider`) + Task 4 (defaulted constructor). ✓
- Inbound-oversize drop+log; oversize-send keyed on `buf.len()` → Task 3. ✓
- E2E `protected` buffer pooled-or-restructured, measured → Task 5. ✓
- Handler-tree flattening, kept only if numbers move → Task 6. ✓
- Doc debt rewrite (`mod.rs:12-30`) → Task 7. ✓
- Validate against PR-0 numbers; drop changes that don't move it → Steps in Tasks 5, 6 + budget retighten in Task 7. ✓
- Deferred readiness-split receive → out of scope, recorded in the design doc; not a task here. ✓

**Placeholder scan:** Task 5/6 contain measurement-derived values (budgets, the A/B E2E choice) that are *intentionally* resolved at implementation time against live witness output — each has an explicit capture-then-decide step, not a vague "TBD." The `protect()` call shape in Task 5 and the `ControlAction` field lists in Task 6 must be matched to the real signatures in `inner.rs`/the E2E registry when those tasks run; flagged inline.

**Type consistency:** `BufferPool`/`BufferLease` (Task 1) → `BufferProvider`/`StaticBufferProvider`/`TokioBufferProvider` (Task 2) → `ClientDeps.buffer_provider: BP` (Task 4) → `socket_loop_future(.., buf: BufferLease)` (Task 3) are consistent across tasks. `Error::Capacity("udp_buffer")` is reused verbatim from the existing send-path error (`socket_manager.rs:447`) for the pool-exhaustion case.

**Risk note carried from the design doc:** the pool hands out `&'static mut [u8]`; Task 1's tests cover claim-to-exhaustion, release-on-drop, and no-double-claim. If `critical-section` is not already satisfiable on the bare-metal build, Task 1 Step 5 adds it to the `bare_metal` feature.
