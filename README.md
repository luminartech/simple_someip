# Simple SOME/IP

[![CI](https://img.shields.io/github/actions/workflow/status/luminartech/simple_someip/ci.yml?style=for-the-badge&label=CI)](https://github.com/luminartech/simple_someip/actions/workflows/ci.yml)
[![Coverage](https://img.shields.io/codecov/c/github/luminartech/simple_someip?style=for-the-badge)](https://app.codecov.io/gh/luminartech/simple_someip)
[![Crates.io](https://img.shields.io/crates/v/simple-someip?style=for-the-badge)](https://crates.io/crates/simple-someip)

Simple SOME/IP is a Rust library implementing the SOME/IP automotive communication protocol — remote procedure calls, event notifications, and wire format serialization. Based on the [Open SOME/IP Specification](https://github.com/some-ip-com/open-someip-spec).

The library supports both `std` and `no_std` environments, making it suitable for embedded targets as well as host-side tooling and scripting.

## Features

- **`no_std` compatible** — `protocol`, `traits`, `transport`, and `e2e` modules work without the standard library
- **Service Discovery** — SD entry/option encoding and decoding via fixed-capacity `heapless` collections (no heap allocation)
- **End-to-End protection** — Profile 4 (CRC-32) and Profile 5 (CRC-16) with zero-allocation APIs
- **Executor-agnostic transport traits** — `TransportSocket`, `TransportFactory`, `Timer`, `Spawner` (default `tokio` impls behind feature gates)
- **Async client and server** — tokio-based, gated behind optional feature flags
- **`embedded-io`** traits for serialization — abstracts over `std::io::Read`/`Write`

## Modules

- `protocol` — Wire format layer: SOME/IP header, `MessageId`, `MessageType`, `ReturnCode`, SD entries/options
- `traits` — `WireFormat` and `PayloadWireFormat` traits for custom message types
- `transport` — Executor-agnostic UDP socket / factory / timer / spawner traits (no_std-compatible)
- `e2e` — End-to-End protection profiles (always available, no heap allocation)
- `tokio_transport` — Default `std + tokio` impls of the transport traits (requires `feature = "client-tokio"` or `feature = "server-tokio"`)
- `client` — High-level async client trait surface (requires `feature = "client"`; add `client-tokio` for the `Client::new` convenience constructor)
- `server` — Async server with SD announcements and event publishing (requires `feature = "server"`; add `server-tokio` for the `Server::new` convenience constructor)

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
# Default — includes std, thiserror, and tracing
simple-someip = "0.8"

# no_std only (protocol/transport/E2E/traits, no heap allocation)
simple-someip = { version = "0.8", default-features = false }

# Client only (with tokio convenience constructors)
simple-someip = { version = "0.8", features = ["client-tokio"] }

# Server only (with tokio convenience constructors)
simple-someip = { version = "0.8", features = ["server-tokio"] }

# Both client and server
simple-someip = { version = "0.8", features = ["client-tokio", "server-tokio"] }
```

### Feature flags

| Feature | Default | Description |
|---------|---------|-------------|
| `std` | **yes** | Enables `thiserror`, `tracing`, and `embedded-io/std` |
| `client` | no | Client trait surface; implies `std` + futures (no tokio) |
| `client-tokio` | no | Adds `Client::new` / `TokioSpawner` / `TokioTransport` defaults; implies `client` + tokio + socket2 |
| `server` | no | Server trait surface; implies `std` + futures (no tokio) |
| `server-tokio` | no | Adds `Server::new` / `TokioTimer` / `TokioTransport` defaults; implies `server` + tokio + socket2 |
| `bare_metal` | no | Activates embassy-sync, no-alloc `static_channels` module, `AtomicInterfaceHandle`, and `StaticE2EHandle`. See `examples/bare_metal_client` and `examples/bare_metal_server`; verify with `cargo build -p bare_metal_client` (NOT `cargo build --workspace`, which can unify features). |
| `embassy_channels` | no | Heap-backed `EmbassySyncChannels` (implies `bare_metal` + `alloc`). Useful for tests before sizing static pools. |

By default the crate enables `std`. To use in a `no_std` environment (e.g., embedded targets), disable default features with `default-features = false`. In that mode the `protocol`, `traits`, `transport`, and `e2e` modules are available; `client` / `server` (and their `tokio_transport` backend) are not. Most applications only need one of `client` or `server`.

## Quick Start

These examples require the `client-tokio` and `server-tokio` features respectively.

### Client

```rust
use simple_someip::{Client, ClientUpdate, RawPayload};
use std::net::Ipv4Addr;

#[tokio::main]
async fn main() {
    // Client::new returns a Clone-able handle, an update stream, and
    // the run-loop future. The future must be actively driven — either
    // spawned on the runtime as shown below, or awaited alongside your
    // own work in a `tokio::select!`. If the future is never polled,
    // Client method calls that send commands over the control channel
    // will hang indefinitely waiting on their oneshot response.
    // `Error::Shutdown` is returned only once the run-loop future has
    // been dropped or its task cancelled.
    let (client, mut updates, run) =
        Client::<RawPayload, _, _, _>::new(Ipv4Addr::new(192, 168, 1, 100));
    let _run_task = tokio::spawn(run);

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
    let announce_handle = tokio::spawn(server.announcement_loop()?);

    let publisher = server.publisher();
    let run_handle = tokio::spawn(async move { server.run().await });

    // Publish events to subscribers, e.g.:
    // publisher.publish_event(0x1234, 1, 0x01, &message).await?;

    tokio::select! {
        res = announce_handle => eprintln!("announcement loop exited unexpectedly: {res:?}"),
        res = run_handle      => eprintln!("server run loop exited: {res:?}"),
    }
    Ok(())
}
```

## Examples

Examples are provided in the `examples/` directory. To run the discovery client example:

```bash
cargo run -p discovery_client
```
