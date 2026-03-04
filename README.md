# Simple SOME/IP

[![CI](https://img.shields.io/github/actions/workflow/status/luminartech/simple_someip/ci.yml?style=for-the-badge&label=CI)](https://github.com/luminartech/simple_someip/actions/workflows/ci.yml)
[![Coverage](https://img.shields.io/codecov/c/github/luminartech/simple_someip?style=for-the-badge)](https://app.codecov.io/gh/luminartech/simple_someip)
[![Crates.io](https://img.shields.io/crates/v/simple-someip?style=for-the-badge)](https://crates.io/crates/simple-someip)

Simple SOME/IP is a Rust library implementing the SOME/IP automotive communication protocol — remote procedure calls, event notifications, and wire format serialization. Based on the [Open SOME/IP Specification](https://github.com/some-ip-com/open-someip-spec).

The library supports both `std` and `no_std` environments, making it suitable for embedded targets as well as host-side tooling and scripting.

## Features

- **`no_std` compatible** — the `protocol`, `traits`, and `e2e` modules work without the standard library
- **Service Discovery** — SD entry/option encoding and decoding via fixed-capacity `heapless` collections (no heap allocation)
- **End-to-End protection** — Profile 4 (CRC-32) and Profile 5 (CRC-16) with zero-allocation APIs
- **Async client and server** — tokio-based, gated behind optional feature flags
- **`embedded-io`** traits for serialization — abstracts over `std::io::Read`/`Write`

## Modules

- `protocol` — Wire format layer: SOME/IP header, `MessageId`, `MessageType`, `ReturnCode`, SD entries/options
- `traits` — `WireFormat` and `PayloadWireFormat` traits for custom message types
- `e2e` — End-to-End protection profiles (always available, no heap allocation)
- `client` — High-level async tokio client (requires `feature = "client"`)
- `server` — Async tokio server with SD announcements and event publishing (requires `feature = "server"`)

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
# Protocol/E2E only (default) — no_std compatible, no tokio dependency
simple-someip = "0.4"

# Client only
simple-someip = { version = "0.4", features = ["client"] }

# Server only
simple-someip = { version = "0.4", features = ["server"] }

# Both client and server
simple-someip = { version = "0.4", features = ["client", "server"] }
```

### Feature flags

| Feature | Default | Description |
|---------|---------|-------------|
| `client` | no | Async tokio client; implies `std` + tokio + socket2 |
| `server` | no | Async tokio server; implies `std` + tokio + socket2 |
| `std` | no | Enables std-dependent code |

By default only the `protocol`, `traits`, and `e2e` modules are available, and the crate compiles in `no_std` mode. Most applications only need one of `client` or `server`.

## Examples

Examples are provided in the `examples/` directory. To run the discovery client example:

```bash
cargo run --features client --example discovery_client
```
