# Simple SOME/IP - Copilot Instructions

## Project Overview

A Rust implementation of the SOME/IP automotive protocol with **dual `no_std`/`std` support**. Core modules (`protocol`, `e2e`, `transport`, `traits`) work without allocation; optional `client`/`server` modules add async tokio networking.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                    Feature-Gated Layers                          │
├─────────────────────────────────────────────────────────────────┤
│ client/server (tokio)  ← requires features = ["client"/"server"]│
│ tokio_transport        ← default std backend                    │
├─────────────────────────────────────────────────────────────────┤
│ transport (traits)     ← executor-agnostic, no_std              │
│ protocol / e2e / traits ← zero-allocation core                  │
└─────────────────────────────────────────────────────────────────┘
```

- **`protocol/`**: Wire format - headers, `MessageId`, `MessageType`, `ReturnCode`, SD entries/options
- **`e2e/`**: End-to-End protection (Profile 4 CRC-32, Profile 5 CRC-16) - always available, no heap
- **`transport.rs`**: Executor-agnostic traits (`TransportSocket`, `Timer`, `Spawner`) - bare-metal integration point
- **`client/`**: Async tokio client with service discovery, subscriptions (feature-gated)
- **`server/`**: Async tokio server with SD announcements, event publishing (feature-gated)

## Feature Flags & Build Commands

```bash
# Default (std only - protocol/e2e/transport/traits)
cargo build

# Client or server features
cargo build --features client
cargo build --features server
cargo build --features client,server

# Bare-metal verification - MUST build in isolation
cargo build -p bare_metal          # NOT --workspace (feature unification)
cargo clippy -p bare_metal

# no_std core modules only
cargo build --no-default-features
cargo clippy --no-default-features -- -D warnings -D clippy::pedantic

# All features (CI standard)
cargo clippy --workspace --all-features -- -D warnings -D clippy::pedantic
```

## Testing

```bash
# Unit tests (parallel-safe)
cargo test --lib

# Integration tests - REQUIRES --test-threads=1 due to SD port sharing
cargo test --test client_server -- --test-threads=1

# Full suite with coverage (CI pattern)
cargo llvm-cov nextest --all-features
```

## Key Patterns

### Zero-Copy Parsing
Use `*View` types for parsing without allocation:
```rust
let view = HeaderView::parse(&buf)?;       // src/protocol/header.rs
let sd_view = SdHeaderView::parse(&buf)?;  // src/protocol/sd/header.rs
```

### WireFormat Trait
All serializable types implement `WireFormat` (see `src/traits.rs`):
```rust
let n = header.encode(&mut buf.as_mut_slice())?;  // returns bytes written
let size = header.required_size();                 // pre-compute buffer size
```

### Client/Server Run Loops
Both require spawning a run-loop future - method calls hang without it:
```rust
let (client, updates, run) = Client::<RawPayload>::new(ip);
let _task = tokio::spawn(run);  // MUST be driven
```

### Hybrid Client+Server
When acting as both, use client's `sd_announcements_loop()` for combined `FindService`+`OfferService` in single SD messages (see `examples/client_server/src/main.rs`).

## Conventions

- **`#![no_std]`** at crate root - `extern crate std` only under `#[cfg(feature = "std")]`
- **`heapless`** collections for SD entries/options - fixed capacity, no heap
- **`embedded-io`** traits for serialization - abstracts over `std::io::Read/Write`
- **`clippy::pedantic`** enforced - see CI workflow
- **IPv4-only transport layer** - `SocketAddrV4` directly, no V6 fallback arm
- **Capacity constants** in `client/inner.rs` control memory footprint (`REQUEST_QUEUE_CAP`, etc.)

## Error Handling

- `Error::Shutdown` - run-loop exited before operation completed
- `Error::Capacity("tag")` - fixed-capacity structure full (e.g., `"pending_responses"`, `"udp_buffer"`)
- E2E check results return `E2ECheckStatus` enum, not errors

## Common Gotchas

1. **Feature unification**: `cargo build --workspace` unifies features - use `-p bare_metal` for bare-metal verification
2. **SD port contention**: Integration tests share multicast port 30490 - must run with `--test-threads=1`
3. **`UDP_BUFFER_SIZE` (1500)**: Application-level limit, not MTU-safe with IP/UDP headers
4. **`Spawner::spawn`** requires `Send + 'static` - unlike socket/timer futures which are executor-agnostic
