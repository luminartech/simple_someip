# Source-Keyed Client Service Registry Implementation Plan

> **Amendment 2026-07-16 (implemented):** the key field and `target_ip`
> parameters shipped as the version-agnostic `core::net::IpAddr`, not
> `Ipv4Addr` as this plan's snippets show. Semantics unchanged (IP-only device
> identity); only the Rust type widened. Transports remain IPv4-only, so only
> V4 keys are auto-registered today.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Key the `simple_someip` client service registry by device IP as well as `(service_id, instance_id)`, so multiple devices advertising the same fixed instance ID are addressed independently.

**Architecture:** The registry map key gains `source_ip: Ipv4Addr`. Because the public methods `subscribe`/`subscribe_no_wait`/`send_to_service`/`remove_endpoint` currently resolve the device *from* the instance-keyed registry, they gain a `target_ip: Ipv4Addr` parameter that threads public API → `ControlMessage` → handler → registry lookup. `add_endpoint` is unchanged (it already carries `addr`; it keys by `addr.ip()`). This is one atomic breaking change (the crate compiles only once every layer is threaded) → version 0.9.0.

**Tech Stack:** Rust 2024, `no_std`-capable crate, `heapless::FnvIndexMap`, `core::net`.

## Global Constraints

- **Key device identity by `Ipv4Addr`** (one IP per device); keep the full `SocketAddrV4` in the entry *value*. No full-`SocketAddrV4` key, no `(service, instance)`-only convenience lookup (rejected in the design — it would resolve 1-vs-many at runtime and re-introduce identity ambiguity).
- **Breaking change → 0.9.0.** `subscribe`, `subscribe_no_wait`, `send_to_service`, `remove_endpoint` gain `target_ip: Ipv4Addr`. `add_endpoint` signature unchanged.
- **Do not touch** the already-per-source machinery: `SessionTracker` (`session.rs`), E2E registry, `ClientUpdate` source plumbing, and the server-side `SubscriptionManager` (keyed by address already). Do not touch the bare-metal / `no_std` codec path.
- Registry must remain heap-free (`heapless::FnvIndexMap`); `SERVICE_REGISTRY_CAP` must stay a power of two.
- Keep the crate warning-clean (`cargo clippy --all-targets -- -D warnings`) and `rustfmt`-clean.
- Design reference: `docs/simple_someip/plans/2026-07-15-source-keyed-registry-design.md`.

---

## File Structure

- `src/client/service_registry.rs` — **modify.** New `ServiceEndpointKey`; `insert`/`get`/`remove` take it; `SERVICE_REGISTRY_CAP` → env-configurable, default 64; unit tests updated + multi-device tests added. Owns the registry data structure.
- `src/client/inner.rs` — **modify.** `ControlMessage::{RemoveEndpoint, SendToService, Subscribe}` gain `target_ip`; their constructors + `Debug` arms; the four handlers; offer auto-registration + `StopOfferService` teardown; the `ServiceInstanceId` import. Owns the client event loop and control plumbing.
- `src/client/mod.rs` — **modify.** Public `subscribe`/`subscribe_no_wait`/`send_to_service`/`remove_endpoint` gain `target_ip`. Owns the public client API.
- `tests/client_server.rs`, `tests/bare_metal_e2e.rs` — **modify.** Update in-repo callers to pass `target_ip`.
- `Cargo.toml`, `CHANGELOG.md` — **modify.** Version bump + changelog (Task 2).

---

## Task 1: Re-key the client registry by device IP (atomic breaking change)

This task is atomic: the crate does not compile until every layer is threaded, so all steps land in one commit. Steps are ordered bottom-up (registry → control plumbing → handlers → public API → callers → tests) so the final `cargo test` is the single verification point.

**Files:** `src/client/service_registry.rs`, `src/client/inner.rs`, `src/client/mod.rs`, `tests/client_server.rs`, `tests/bare_metal_e2e.rs`

**Interfaces produced (the names later steps and dft depend on):**
- `ServiceEndpointKey { service_id: u16, instance_id: u16, source_ip: core::net::Ipv4Addr }` (derives `Clone, Copy, Debug, Eq, Hash, PartialEq`).
- `ServiceRegistry::{insert,get,remove}(&… , key: ServiceEndpointKey, …)`.
- Public API gains `target_ip: Ipv4Addr` as described below.

- [ ] **Step 1: Registry — new key type, methods, capacity**

In `src/client/service_registry.rs`:

Replace the `SERVICE_REGISTRY_CAP` const (line 10) and the `ServiceInstanceId` struct (lines 12-16) with:

```rust
pub const SERVICE_REGISTRY_CAP: usize =
    crate::from_env_or(option_env!("SIMPLE_SOMEIP_SERVICE_REGISTRY_CAP"), 64);

/// Identifies one service instance ON A SPECIFIC DEVICE. The device IP is part
/// of the key because a fixed (ECU-Extract) instance id is shared by every
/// device on the subnet — keying without the source IP would collapse all of
/// them onto one entry (last-writer-wins). Mirrors how `SessionTracker` keys
/// per device by address.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ServiceEndpointKey {
    pub service_id: u16,
    pub instance_id: u16,
    pub source_ip: core::net::Ipv4Addr,
}
```

Change the map field (line 30) to key on the new type:

```rust
    endpoints: FnvIndexMap<ServiceEndpointKey, ServiceEndpointInfo, SERVICE_REGISTRY_CAP>,
```

Change the three method signatures to take `ServiceEndpointKey` (bodies otherwise unchanged — they already delegate to the map):

```rust
    pub fn insert(&mut self, key: ServiceEndpointKey, info: ServiceEndpointInfo)
        -> Result<(), ServiceRegistryFull> {
        self.endpoints.insert(key, info).map(|_| ()).map_err(|_| ServiceRegistryFull)
    }
    pub fn remove(&mut self, key: ServiceEndpointKey) -> Option<ServiceEndpointInfo> {
        self.endpoints.swap_remove(&key)
    }
    pub fn get(&self, key: ServiceEndpointKey) -> Option<&ServiceEndpointInfo> {
        self.endpoints.get(&key)
    }
```

- [ ] **Step 2: Registry — rewrite the unit tests to the new key + add multi-device coverage**

Replace the `#[cfg(test)] mod tests` (lines 62-153) test helpers and cases so they use `ServiceEndpointKey`. Use distinct loopback IPs to represent distinct devices:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use core::net::Ipv4Addr;

    fn key(service: u16, instance: u16, ip: Ipv4Addr) -> ServiceEndpointKey {
        ServiceEndpointKey { service_id: service, instance_id: instance, source_ip: ip }
    }
    fn info(ip: Ipv4Addr, port: u16) -> ServiceEndpointInfo {
        ServiceEndpointInfo { addr: SocketAddrV4::new(ip, port), local_port: 0, major_version: 1, minor_version: 0 }
    }
    const A: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);
    const B: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 2);

    #[test]
    fn insert_and_get() {
        let mut reg = ServiceRegistry::default();
        reg.insert(key(0x47, 54, A), info(A, 30000)).unwrap();
        assert_eq!(reg.get(key(0x47, 54, A)).unwrap().addr.port(), 30000);
    }

    #[test]
    fn two_devices_same_service_instance_coexist() {
        // The regression: identical (service, instance) from different device IPs
        // must both persist and resolve independently.
        let mut reg = ServiceRegistry::default();
        reg.insert(key(0x47, 54, A), info(A, 30001)).unwrap();
        reg.insert(key(0x47, 54, B), info(B, 30002)).unwrap();
        assert_eq!(reg.get(key(0x47, 54, A)).unwrap().addr, SocketAddrV4::new(A, 30001));
        assert_eq!(reg.get(key(0x47, 54, B)).unwrap().addr, SocketAddrV4::new(B, 30002));
    }

    #[test]
    fn reinsert_same_key_replaces_in_place() {
        let mut reg = ServiceRegistry::default();
        reg.insert(key(0x47, 54, A), info(A, 30000)).unwrap();
        reg.insert(key(0x47, 54, A), info(A, 40000)).unwrap();
        assert_eq!(reg.get(key(0x47, 54, A)).unwrap().addr.port(), 40000);
    }

    #[test]
    fn remove_one_device_leaves_the_other() {
        // Regression for "StopOffer from one device evicted all".
        let mut reg = ServiceRegistry::default();
        reg.insert(key(0x47, 54, A), info(A, 30001)).unwrap();
        reg.insert(key(0x47, 54, B), info(B, 30002)).unwrap();
        assert!(reg.remove(key(0x47, 54, A)).is_some());
        assert!(reg.get(key(0x47, 54, A)).is_none());
        assert!(reg.get(key(0x47, 54, B)).is_some());
    }

    #[test]
    fn get_missing_returns_none() {
        let reg = ServiceRegistry::default();
        assert!(reg.get(key(0xFFFF, 0xFFFF, A)).is_none());
    }

    #[test]
    fn remove_missing_returns_none() {
        let mut reg = ServiceRegistry::default();
        assert!(reg.remove(key(0xFFFF, 0xFFFF, A)).is_none());
    }

    #[test]
    fn insert_returns_full_at_cap() {
        let mut reg = ServiceRegistry::default();
        for i in 0..SERVICE_REGISTRY_CAP {
            #[allow(clippy::cast_possible_truncation)]
            let k = key(i as u16, 0, A);
            assert!(reg.insert(k, info(A, 0)).is_ok());
        }
        assert_eq!(reg.insert(key(0xFFFF, 0xFFFF, A), info(A, 0)), Err(ServiceRegistryFull));
    }

    #[test]
    fn insert_at_cap_for_existing_key_succeeds() {
        let mut reg = ServiceRegistry::default();
        for i in 0..SERVICE_REGISTRY_CAP {
            #[allow(clippy::cast_possible_truncation)]
            let k = key(i as u16, 0, A);
            assert!(reg.insert(k, info(A, 0)).is_ok());
        }
        assert!(reg.insert(key(0, 0, A), info(A, 9999)).is_ok());
        assert_eq!(reg.get(key(0, 0, A)).unwrap().addr.port(), 9999);
    }
}
```

- [ ] **Step 3: `ControlMessage` — add `target_ip` to the three routed variants**

In `src/client/inner.rs`, update the enum (lines 60-78):

```rust
    RemoveEndpoint(u16, u16, Ipv4Addr, C::OneshotSender<Result<(), Error>>),
    SendToService {
        service_id: u16,
        instance_id: u16,
        target_ip: Ipv4Addr,
        message: Message<P>,
        send_complete: C::OneshotSender<Result<(), Error>>,
        response: C::OneshotSender<Result<P, Error>>,
    },
    Subscribe {
        service_id: u16,
        instance_id: u16,
        target_ip: Ipv4Addr,
        major_version: u8,
        ttl: u32,
        event_group_id: u16,
        client_port: u16,
        response: C::OneshotSender<Result<(), Error>>,
    },
```

(`AddEndpoint` unchanged — it already carries `SocketAddrV4`.) `Ipv4Addr` is already imported in this file (used by `SetInterface`).

- [ ] **Step 4: `ControlMessage` — Debug arms**

Update the `RemoveEndpoint`, `SendToService`, `Subscribe` `Debug` arms (lines 104-130) to include `target_ip`:

```rust
            Self::RemoveEndpoint(sid, iid, target_ip, _) => f
                .debug_tuple("RemoveEndpoint").field(sid).field(iid).field(target_ip).finish(),
            Self::SendToService { service_id, instance_id, target_ip, message, .. } => f
                .debug_struct("SendToService")
                .field("service_id", service_id)
                .field("instance_id", instance_id)
                .field("target_ip", target_ip)
                .field("message", message)
                .finish_non_exhaustive(),
            Self::Subscribe { service_id, instance_id, target_ip, event_group_id, .. } => f
                .debug_struct("Subscribe")
                .field("service_id", service_id)
                .field("instance_id", instance_id)
                .field("target_ip", target_ip)
                .field("event_group_id", event_group_id)
                .finish_non_exhaustive(),
```

- [ ] **Step 5: `ControlMessage` constructors**

Update the three constructors (lines 188-245) to accept and forward `target_ip`:

- `remove_endpoint(service_id, instance_id, target_ip: Ipv4Addr)` → `Self::RemoveEndpoint(service_id, instance_id, target_ip, sender)`.
- `send_to_service(service_id, instance_id, target_ip: Ipv4Addr, message)` → set `target_ip,` in the `SendToService { … }` literal.
- `subscribe(service_id, instance_id, target_ip: Ipv4Addr, major_version, ttl, event_group_id, client_port)` → set `target_ip,` in the `Subscribe { … }` literal.

Keep the `target_ip` parameter positioned immediately after `instance_id` in every signature (consistent ordering across the whole change).

- [ ] **Step 6: `inner.rs` — import the new key type**

Change the import (line 20) from `ServiceInstanceId` to `ServiceEndpointKey`:

```rust
        service_registry::{ServiceEndpointInfo, ServiceEndpointKey, ServiceRegistry},
```

- [ ] **Step 7: `inner.rs` — offer auto-registration + teardown key by device IP**

In `handle_discovery_datagram` (around lines 667-698), build the key from the offered endpoint's IP. Replace the `ServiceInstanceId { service_id: ep.service_id, instance_id: ep.instance_id }` construction with:

```rust
            let key = ServiceEndpointKey {
                service_id: ep.service_id,
                instance_id: ep.instance_id,
                source_ip: *addr.ip(), // `addr` is the offered endpoint (ep.addr)
            };
```

Use `key` for both the `is_offer` `service_registry.insert(key, …)` and the `StopOfferService` `service_registry.remove(key)` paths. (For `StopOfferService`, the `ep.addr` is the endpoint being withdrawn, so `*ep.addr.ip()` scopes the removal to that one device — the fix for "one stop evicts all".) If `ep.addr` is only bound inside the `is_offer` arm, hoist the `source_ip = ep.addr.map(|a| *a.ip())` extraction so the stop-path can key by it; if a stop entry has no endpoint option, skip removal (cannot identify the device) and log at `debug`.

- [ ] **Step 8: `inner.rs` — `AddEndpoint` handler keys by `addr.ip()`**

In the `ControlMessage::AddEndpoint(service_id, instance_id, addr, local_port, response)` handler (around line 897), build the key from `addr`:

```rust
                    let key = ServiceEndpointKey { service_id, instance_id, source_ip: *addr.ip() };
                    let insert_result = self.service_registry.insert(
                        key,
                        ServiceEndpointInfo { addr, local_port, major_version: 0xFF, minor_version: 0xFFFF_FFFF },
                    );
```

- [ ] **Step 9: `inner.rs` — `RemoveEndpoint` handler**

Destructure the new `target_ip` and key by it (around line 935):

```rust
                ControlMessage::RemoveEndpoint(service_id, instance_id, target_ip, response) => {
                    self.service_registry.remove(ServiceEndpointKey { service_id, instance_id, source_ip: target_ip });
                    // …existing response-send unchanged…
                }
```

- [ ] **Step 10: `inner.rs` — `SendToService` handler**

Destructure `target_ip` and build the lookup key (around lines 948-963):

```rust
                ControlMessage::SendToService { service_id, instance_id, target_ip, message, send_complete, response } => {
                    let key = ServiceEndpointKey { service_id, instance_id, source_ip: target_ip };
                    let Some(endpoint) = self.service_registry.get(key) else {
                        let _ = send_complete.send(Err(Error::ServiceNotFound));
                        return; // (preserve the existing control-flow shape at this site)
                    };
                    let target = endpoint.addr;
                    // …rest unchanged…
                }
```

(Match the existing site's exact control flow — this shows only the key/lookup change.)

- [ ] **Step 11: `inner.rs` — `Subscribe` handler**

Destructure `target_ip` (around line 1046) and use it for the registry lookup that currently builds `ServiceInstanceId` (line 1056) and for the re-enqueue path (line 1100, which reconstructs the `Subscribe` message — carry `target_ip` through). The lookup at line 1139 (`self.service_registry.get(id)`) becomes `self.service_registry.get(ServiceEndpointKey { service_id, instance_id, source_ip: target_ip })`. The subscribe destination continues to use the SD port (`SocketAddrV4::new(*reg.addr.ip(), protocol::sd::MULTICAST_PORT)`), unchanged — only the entry it resolves is now device-scoped.

- [ ] **Step 12: `mod.rs` — public API signatures**

In `src/client/mod.rs`, add `target_ip: Ipv4Addr` (immediately after `instance_id`) to:
- `subscribe` (line 949) and forward to `ControlMessage::subscribe(service_id, instance_id, target_ip, major_version, ttl, event_group_id, client_port)`.
- `subscribe_no_wait` (line 1001) — same.
- `remove_endpoint` (line 1145) → `ControlMessage::remove_endpoint(service_id, instance_id, target_ip)`.
- `send_to_service` (line 1178) → `ControlMessage::send_to_service(service_id, instance_id, target_ip, message)`.

`add_endpoint` (line 1120) unchanged. Ensure `Ipv4Addr` is imported in `mod.rs` (it is — `add_endpoint` uses `SocketAddrV4` from `std::net`/`core::net`; add `Ipv4Addr` to that use if not already present). Update the doc-comment/examples on these four methods to pass the new argument.

- [ ] **Step 13: Update in-repo callers**

`tests/client_server.rs` — every `Client::subscribe`/`send_to_service`/`remove_endpoint` call gains the server's IP as `target_ip` (the tests already bind the server to a known `SocketAddrV4`; use `*server_addr.ip()` or the literal loopback the test uses). Sites: subscribe at lines 172, 257, 295, 357, 404, 481, 546, 558, 652, 680, 685; `remove_endpoint` at 324; `send_to_service` at 326, 420. Transformation, e.g.:

```rust
// before
.subscribe(service_id, 1, 1, 3, 0x01, 0)
// after (target is the server this test talks to)
.subscribe(service_id, 1, SERVER_IP, 1, 3, 0x01, 0)
```
where `SERVER_IP` is the `Ipv4Addr` the test's server is bound to (introduce a `const`/`let` if the test doesn't already name it).

`tests/bare_metal_e2e.rs` — `Client::send_to_service` at lines 549, 969, 1078 gain the server IP. (The `.subscribe(0xABCD, 1, 0x01, subscriber_addr)` and `subs.subscribe(...)` calls in this file and in `event_publisher.rs`/`subscription_manager.rs`/`no_alloc_server_witness.rs` are the **server-side** `SubscriptionManager`/`SubscriptionHandle` — 3-4 args — and must NOT be changed.)

- [ ] **Step 14: Add a client-level regression test**

Add to `tests/client_server.rs` a test that two servers bound to distinct loopback IPs, both offering the same `(service_id, instance_id)`, are independently reachable: subscribe/`send_to_service` to `target_ip = A` reaches server A and to `B` reaches server B (assert responses come from the right server), and removing A's endpoint leaves B reachable. Model it on the existing multi-subscriber/two-server tests already in the file (reuse their harness/setup). If binding two servers is impractical in the existing harness, assert the narrower property: after `add_endpoint(svc, inst, addr_A, _)` then `add_endpoint(svc, inst, addr_B, _)`, a `send_to_service(svc, inst, A, …)` targets A and `(…, B, …)` targets B (observe the destination), proving both entries coexist.

- [ ] **Step 15: fmt, clippy, test**

Run:
```
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
```
Expected: clean fmt, no clippy warnings, all tests pass (registry unit tests incl. the new multi-device ones; `client_server` incl. the new regression test).

- [ ] **Step 16: Commit**

```bash
git add src/client/service_registry.rs src/client/inner.rs src/client/mod.rs tests/client_server.rs tests/bare_metal_e2e.rs
git commit -m "feat(client)!: key service registry by device IP

The client ServiceRegistry keyed endpoints by (service_id, instance_id) only,
which collapsed multiple devices onto one slot once firmware advertised a fixed
(ECU-Extract) instance id. Add the device IP to the key; subscribe/subscribe_no_wait/
send_to_service/remove_endpoint gain a target_ip param. add_endpoint keys by addr.ip().
SERVICE_REGISTRY_CAP is now env-configurable (default 64).

BREAKING CHANGE: subscribe/subscribe_no_wait/send_to_service/remove_endpoint take target_ip.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Version bump + CHANGELOG

**Files:** `Cargo.toml`, `CHANGELOG.md`

- [ ] **Step 1: Bump the version**

In `Cargo.toml`, change `version = "0.8.0"` → `version = "0.9.0"`.

- [ ] **Step 2: CHANGELOG entry**

Add a `## 0.9.0` section at the top of `CHANGELOG.md` (match the file's existing heading/format — read the top of the file first):

```markdown
## 0.9.0

### Breaking
- Client service registry is now keyed by `(service_id, instance_id, device IP)`
  instead of `(service_id, instance_id)`, so multiple devices advertising the same
  fixed instance id are tracked and addressed independently. `Client::subscribe`,
  `subscribe_no_wait`, `send_to_service`, and `remove_endpoint` gain a
  `target_ip: Ipv4Addr` parameter identifying the device. `add_endpoint` is
  unchanged.

### Added
- `SIMPLE_SOMEIP_SERVICE_REGISTRY_CAP` build-time env override for the client
  registry capacity (default 64).
```

- [ ] **Step 3: Verify build + commit**

Run: `cargo build` (confirm version bump doesn't break anything). Then:
```bash
git add Cargo.toml CHANGELOG.md
git commit -m "chore(release): 0.9.0 — source-keyed client registry"
```

---

## Self-Review

**Spec coverage:**
- Registry keyed by device IP → Task 1 Steps 1, 7, 8.
- `target_ip` on subscribe/subscribe_no_wait/send_to_service/remove_endpoint → Steps 3-5, 9-12.
- `add_endpoint` unchanged → Step 8 (keys by `addr.ip()`, signature untouched).
- `StopOfferService` removes only that device → Step 7.
- Capacity env-configurable, default 64, power of two → Step 1.
- Untouched per-source machinery / server / bare-metal → Global Constraints + Step 13 note (server-side `subscribe` callers explicitly excluded).
- Tests incl. multi-device coexistence, remove-one-leaves-other, capacity → Steps 2, 14.
- Semver 0.9.0 + CHANGELOG → Task 2.
- dft downstream migration is out of scope (design §Downstream) — not a task here.

**Placeholder scan:** Steps 10-11 intentionally show only the key/lookup change against the existing handler bodies (the implementer edits in-place against real surrounding code, which the plan cannot fully reproduce without copying large unchanged blocks) — each names the exact line and the exact key construction, which is the whole change at that site.

**Type consistency:** `ServiceEndpointKey { service_id: u16, instance_id: u16, source_ip: Ipv4Addr }` is defined once (Step 1) and constructed identically in Steps 7-11. `target_ip: Ipv4Addr` is positioned immediately after `instance_id` in every signature (enum, constructors, public API). `get`/`insert`/`remove` take `ServiceEndpointKey` everywhere.
