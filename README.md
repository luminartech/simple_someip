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
simple-someip = "0.5"

# Client only
simple-someip = { version = "0.5", features = ["client"] }

# Server only
simple-someip = { version = "0.5", features = ["server"] }

# Both client and server
simple-someip = { version = "0.5", features = ["client", "server"] }
```

### Feature flags

| Feature | Default | Description |
|---------|---------|-------------|
| `client` | no | Async tokio client; implies `std` + tokio + socket2 |
| `server` | no | Async tokio server; implies `std` + tokio + socket2 |
| `std` | no | Enables std-dependent code |

By default only the `protocol`, `traits`, and `e2e` modules are available, and the crate compiles in `no_std` mode. Most applications only need one of `client` or `server`.

## Quick Start

### Client

```rust
use simple_someip::{Client, ClientUpdate, RawPayload};
use std::net::Ipv4Addr;

#[tokio::main]
async fn main() {
    // Client::new returns a (Client, ClientUpdates) pair.
    // Client is Clone-able and can be shared across tasks.
    let (client, mut updates) = Client::<RawPayload>::new(Ipv4Addr::new(192, 168, 1, 100));

    // Bind the SD multicast socket to discover services
    client.bind_discovery().await.unwrap();

    // Receive discovery, unicast, and error updates
    while let Some(update) = updates.recv().await {
        match update {
            ClientUpdate::DiscoveryUpdated(msg) => { /* SD message */ }
            ClientUpdate::Unicast { message, e2e_status } => { /* unicast reply */ }
            ClientUpdate::SenderRebooted(addr) => { /* remote reboot detected */ }
            ClientUpdate::Error(err) => { /* error */ }
        }
    }
}
```

### Server

```rust
use simple_someip::server::{Server, ServerConfig};
use std::net::Ipv4Addr;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = ServerConfig::new(Ipv4Addr::new(192, 168, 1, 200), 30500, 0x1234, 1);
    let mut server = Server::new(config).await?;
    server.start_announcing()?;

    let publisher = server.publisher();
    tokio::spawn(async move { server.run().await });

    // Publish events to subscribers...
    Ok(())
}
```

## Examples

Examples are provided in the `examples/` directory. To run the discovery client example:

```bash
cargo run -p discovery_client
```
