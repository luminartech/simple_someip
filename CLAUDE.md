# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

simple-someip: A Rust library implementing the SOME/IP automotive communication protocol (remote procedure calls, event notifications, wire format serialization). Based on the [Open SOME/IP Specification](https://github.com/some-ip-com/open-someip-spec).

## Commands

```bash
# Build
cargo build --all-features

# Run all tests
cargo test --all-features

# Run a single test
cargo test --all-features test_name

# Format check
cargo fmt --check

# Lint (must pass with zero warnings — CI enforces this)
cargo clippy --all-features -- -D warnings -D clippy::pedantic

# Coverage (requires cargo-llvm-cov)
cargo llvm-cov --all-features --lcov --output-path lcov.info
```

## Architecture

### Module Structure

- **`protocol/`** — Wire format layer. SOME/IP header (16-byte), `MessageId`, `MessageType`, `ReturnCode`, byte-order helpers. `sd/` sub-module handles Service Discovery entries, flags, and options. `tp/` is a placeholder for SOME/IP-TP.
- **`traits`** — `WireFormat` (encode/decode via `embedded-io`) and `PayloadWireFormat` (higher-level payload abstraction). Custom message types implement these traits.
- **`client/`** — Async tokio client. `Client<P: PayloadWireFormat>` drives an internal task (`inner.rs`) that manages discovery (multicast) and unicast sockets via `socket_manager.rs`. Gated by `feature = "client"`.
- **`server/`** — Async tokio server. Handles SD announcements, subscription management, and event publishing. Gated by `feature = "server"`.
- **`e2e/`** — End-to-End protection (Profile 4: CRC-32, 12-byte header; Profile 5: CRC-16, 3-byte header). Zero-allocation: protect/check functions write into caller-provided `&mut [u8]` buffers and return `Result`.

### Key Design Decisions

- **`heapless`** for SD entry/option collections — fixed-capacity, no heap allocation in protocol layer.
- **`embedded-io`** traits for serialization — abstracts over `std::io::Read`/`Write`.
- **E2E functions avoid heap allocation** — they take output buffers by reference, not `Vec`. The `crc` dependency is pinned to `>=3, <3.4` because 3.4.0 introduced a breaking `Digest<u16>::update` signature change.
- **Feature flags**: `default = ["client", "server"]`. `client` pulls in tokio; `server` has no extra deps. `e2e` and `protocol` are always available.

## CI

- Clippy pedantic + rustfmt enforced on every push/PR/merge-group
- Linear history enforced on PRs (no merge commits — rebase only)
- Coverage via `cargo-llvm-cov` reported to Codecov (80% target for project and patch)

## Conventions

- Clippy pedantic is enabled project-wide (`#![warn(clippy::pedantic)]` in `lib.rs`)
- Server tests use a mutex (`SD_PORT_LOCK`) for serial execution due to shared multicast port
- No panics in library code — functions return `Result<T, Error>`
