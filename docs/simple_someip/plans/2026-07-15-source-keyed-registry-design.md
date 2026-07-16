# Source-Keyed Client Service Registry Design

**Date:** 2026-07-15
**Crate:** `simple-someip` (the `client` feature)
**Target version:** 0.9.0 (breaking client API change)
**Status:** Approved design, pending implementation plan

## Summary

The client-side `ServiceRegistry` keys a service instance's routable endpoint by
`(service_id, instance_id)` **alone** (`src/client/service_registry.rs:12`). That
was fine only because Iris firmware used to derive each sensor's SOME/IP instance
ID from its IP's last octet, making the instance ID unique per device. That
runtime override was removed (`iris_firmware` `c17fa53`); instance IDs now come
fixed from ECU-Extract, so **every** sensor on a subnet advertises the same
`(service_id, instance_id)` (e.g. `(0x47, 54)`). Multiple devices therefore
collapse onto one registry slot — last-writer-wins — so `subscribe`,
`send_to_service`, and `remove_endpoint` all resolve to whichever device offered
most recently, and a `StopOfferService` from any one device evicts the entry for
all.

This design makes the registry key the device by its **IP address** in addition
to `(service_id, instance_id)`, so multiple devices sharing an instance ID are
tracked independently. It mirrors how `SessionTracker` already keys per device by
`SocketAddr` (`src/client/session.rs:24`).

## Scope of the collision (why only the registry)

Confirmed by audit — the registry is the *only* client-side identity that omits
the source address. These are already per-source and need **no change**:

- Session / reboot tracking — `SessionTracker` keyed by `(SocketAddr, transport,
  service_id, instance_id)` (`session.rs:24`).
- E2E receive state — reset per source IP (`e2e/registry.rs:210`).
- `ClientUpdate::{DiscoveryUpdated, SenderRebooted}` — carry `source`.
- Server-side `SubscriptionManager` — keyed by `(service, instance, eventgroup)`
  → a *list* of per-`SocketAddrV4` subscribers (`subscription_manager.rs:88`).

The bare-metal / `no_std` codec path does not use this registry; it is a
`client`-feature (std) structure only.

## Locked-in decisions

> **Amendment 2026-07-16:** the key/parameter type shipped as the
> version-agnostic `core::net::IpAddr`, not `Ipv4Addr` as written below. The
> identity semantics are unchanged (one IP per device, IP-only — not
> `SocketAddr` — keying); only the Rust type widened so the client API doesn't
> bake in IPv4 while the SD protocol layer already parses IPv6 endpoint
> options. Today's transports remain IPv4-only, so only V4 keys are ever
> auto-registered.
>
> **Amendment 2 (2026-07-16):** the public addressing API shipped taking
> `ServiceEndpointKey` itself (re-exported at the crate root, with a `new`
> constructor) instead of loose `service_id` / `instance_id` / `target_ip`
> parameters — `subscribe(key, major_version, ttl, event_group_id,
> client_port)`, `remove_endpoint(key)`, `send_to_service(key, message)`,
> `request(key, message)`. The `ControlMessage` variants carry the key too.
> This supersedes this doc's per-method parameter tables; the key's *contents*
> are exactly as designed below.

- **Key device identity by `Ipv4Addr`, not full `SocketAddrV4`.** One IP per
  physical device; the port for a given service is fixed and is retained in the
  entry *value*. IP-only keying is production-correct, updates in place when a
  device re-offers a service on a new port (no stale ghost entries), and matches
  dft's existing per-sensor identity (`iris@<ip>`). Multi-sim tests distinguish
  instances via distinct loopback IPs (`127.0.0.1`, `127.0.0.2`, … — all of
  `127.0.0.0/8` is loopback), so IP keying is fully testable.
  - Rejected: keying by full `SocketAddrV4`. Only helps if one IP legitimately
    hosts multiple instances of the *same* service (not the case here) and it
    risks stale entries on port change.
- **Breaking API change → 0.9.0.** `subscribe`, `subscribe_no_wait`,
  `send_to_service`, and `remove_endpoint` gain a `target_ip: Ipv4Addr`
  parameter. `add_endpoint` is unchanged (it already takes `addr: SocketAddrV4`;
  it keys by `addr.ip()`).
- **The crate change is self-contained.** dft's migration to the new signatures
  is downstream work done when dft upgrades off 0.7.3 (see Downstream).

## Architecture

### Registry key and value

`src/client/service_registry.rs`:

- Key becomes `ServiceEndpointKey { service_id: u16, instance_id: u16, source_ip:
  Ipv4Addr }` (derives `Clone, Copy, Debug, Eq, Hash, PartialEq`). Keep
  `ServiceInstanceId { service_id, instance_id }` as the logical service-identity
  type where callers still reason in those terms.
- `ServiceEndpointInfo` (the value) is unchanged — it already holds the full
  `addr: SocketAddrV4`, `local_port`, and versions. `source_ip` in the key is
  `addr.ip()`.
- `insert` / `get` / `remove` take the new key. `insert` still replaces on an
  exact key match (a device re-offering the same service updates its own entry)
  and returns `ServiceRegistryFull` at capacity.

### Registry population and teardown

`src/client/inner.rs`:

- Auto-registration from `OfferService` (`inner.rs:667-696`): build the key from
  `ep.service_id`, `ep.instance_id`, and `ep.addr.ip()`. Two devices offering the
  same `(service, instance)` now create two entries.
- `StopOfferService` (`inner.rs:697-698`): remove only the entry for that
  device's IP — one device going away no longer evicts the others.
- `AddEndpoint` handler (`inner.rs:897-915`): key by the provided `addr.ip()`.

### Addressing API (breaking)

`src/client/mod.rs` + the `ControlMessage` plumbing + `inner.rs` handlers:

| Method | Before | After |
|---|---|---|
| `subscribe` | `(service_id, instance_id, major_version, ttl, event_group_id, client_port)` | `(service_id, instance_id, target_ip, major_version, ttl, event_group_id, client_port)` |
| `subscribe_no_wait` | same as `subscribe` | + `target_ip` |
| `send_to_service` | `(service_id, instance_id, message)` | `(service_id, instance_id, target_ip, message)` |
| `remove_endpoint` | `(service_id, instance_id)` | `(service_id, instance_id, target_ip)` |
| `add_endpoint` | `(service_id, instance_id, addr, local_port)` | **unchanged** (keys by `addr.ip()`) |

- Lookups (`send_to_service` at `inner.rs:955`, `subscribe` at `inner.rs:1056`,
  `:1139`) match the exact `ServiceEndpointKey`; a miss returns
  `Error::ServiceNotFound` as today.
- `subscribe`'s existing behavior of sending the `SubscribeEventgroup` to the
  device's IP on the SD port is preserved — it just resolves the *device* by
  `target_ip` now instead of by whichever entry last won the `(service,
  instance)` slot.
- `ControlMessage::{subscribe, subscribe_no_wait, send_to_service,
  remove_endpoint}` constructors and their `inner.rs` handlers thread
  `target_ip` through.

### Capacity (`no_std`)

`SERVICE_REGISTRY_CAP` (`service_registry.rs:10`) is now sized by
*devices × services* rather than *services*. Change it from a fixed `32` to the
crate's existing env-configurable pattern and raise the default:

```rust
pub const SERVICE_REGISTRY_CAP: usize =
    crate::from_env_or(option_env!("SIMPLE_SOMEIP_SERVICE_REGISTRY_CAP"), 64);
```

(as with `SIMPLE_SOMEIP_MAX_OFFERS` in `bare_metal_runtime/runtime.rs:40`). 64 is
a power of two — a `FnvIndexMap` requirement — covering ~12 devices × 5 services;
bare-metal callers can size it down (must stay a power of two). Full-registry
behavior (`ServiceRegistryFull`) is unchanged.

## Downstream (dft — separate, on the 0.9.0 upgrade; not this crate's PR)

dft's `iris_someip_client` passes the sensor IP it already tracks (`iris@<ip>`)
into `subscribe`/`send_to_service`/`remove_endpoint`, and its
`known_endpoints: HashMap<IrisService, u16>` becomes per-sensor
(`HashMap<(IrisService, Ipv4Addr), u16>` or keyed by sensor). The already-merged
"trust the offer" change (dft PR #1004) remains valid and necessary — it is not
superseded by this; it is the piece that reads the offered instance, and this is
the piece that keeps multiple devices distinct.

## Testing

Unit tests in `service_registry.rs` and `inner.rs` (all `no_std`-friendly, using
distinct loopback IPs to represent distinct devices):

- Two `insert`s with identical `(service_id, instance_id)` but different
  `source_ip` both persist; `get` returns the correct one per IP.
- Re-`insert` for the same `(service, instance, ip)` replaces in place (no
  duplicate, no stale entry when the value's port changes).
- `remove` for one device's key leaves the other device's entry intact
  (regression for the "StopOffer evicts all" bug).
- Auto-registration from an SD payload carrying two offers of the same
  `(service, instance)` from different endpoint IPs yields two entries.
- `send_to_service` / `subscribe` route to the endpoint matching `target_ip`, and
  return `ServiceNotFound` for an unknown `target_ip`.
- Capacity: filling to `SERVICE_REGISTRY_CAP` then inserting a new key returns
  `ServiceRegistryFull`; inserting an existing key at capacity still succeeds.
- Update existing registry/client tests to the new key and signatures
  (`overwrite_replaces_info` becomes a same-key-replaces test).

## Semver / release

- Bump to **0.9.0**. CHANGELOG entry: breaking — `subscribe`, `subscribe_no_wait`,
  `send_to_service`, and `remove_endpoint` gain `target_ip: Ipv4Addr`; the client
  service registry now tracks one endpoint per `(service, instance, device IP)`
  instead of one per `(service, instance)`, so multiple devices sharing a fixed
  instance ID are addressed independently. New `SIMPLE_SOMEIP_SERVICE_REGISTRY_CAP`
  build-time env override (default 64).

## Resolved decisions

- **No `(service, instance)`-only convenience lookup.** Considered and rejected:
  it could only resolve 1-vs-many at *runtime* (scan matches; error if >1), which
  makes single-sensor code compile and pass, then flip to an `Ambiguous` error the
  day a second device appears — a latent re-introduction of the identity-ambiguity
  bug this change fixes. Callers must pass `target_ip` explicitly (dft always has
  it). If a future consumer needs "act on all instances," expose plurality
  directly (`endpoints_for(service, instance) -> impl Iterator`) rather than
  auto-picking one.
