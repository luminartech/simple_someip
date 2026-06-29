# simple-someip-embassy-net

[embassy-net]-backed `TransportFactory` / `TransportSocket` adapter for
the [`simple-someip`] crate.

This is the **reference no_std backend** for `simple-someip`'s
transport-trait surface. It lets bare-metal Rust embedded projects
running on [embassy-executor] + embassy-net pick up SOME/IP service
discovery and request/response messaging as a one-line dependency
add, without writing their own transport adapter.

## Status

Reference adapter implementing the full `TransportFactory` /
`TransportSocket` surface, with a host loopback integration test and
an in-tree example.

## Quick sketch

```rust,ignore
use simple_someip::{Client, ClientDeps};
use simple_someip_embassy_net::{EmbassyNetFactory, SocketPool};

static SOCKET_POOL: SocketPool<8, 1500, 1500> = SocketPool::new();

#[embassy_executor::main]
async fn main(spawner: embassy_executor::Spawner) {
    let stack = /* ... build embassy-net Stack ... */;
    let factory = EmbassyNetFactory::new(stack, &SOCKET_POOL);

    let (client, _updates, run_fut) = Client::<_, _, _, _>::new_with_deps(
        ClientDeps {
            factory,
            spawner,           // embassy_executor::Spawner
            timer: EmbassyTimer,
            e2e_registry: /* StaticE2EHandle */,
            interface: /* AtomicInterfaceHandle */,
        },
        false, // multicast_loopback
    );
    spawner.spawn(run_fut).unwrap();
    // ... use the client ...
}
```

## License

MIT OR Apache-2.0, matching `simple-someip`.

[embassy-net]: https://crates.io/crates/embassy-net
[embassy-executor]: https://crates.io/crates/embassy-executor
[`simple-someip`]: https://crates.io/crates/simple-someip
