# Changelog

## [0.8.0]

### Added

- **`client::Error::Capacity(&'static str)`** — new variant returned when a fixed-capacity internal structure is full. Current tags: `"unicast_sockets"`, `"udp_buffer"`, `"pending_responses"`, `"request_queue"`. Because `client::Error` is not `#[non_exhaustive]`, this is a breaking change for downstream crates that match the enum exhaustively.
- **`client::Error::Transport(crate::transport::TransportError)`** — new variant surfacing failures from the pluggable transport backend (`#[from]`-converted, displays transparently). Same exhaustive-match caveat as above.
- **`client::Error::Shutdown`** — new variant returned by every `Client` method when the control channel is closed (run-loop future was dropped, cancelled, or exited). Replaces the previous `.unwrap()`-on-closed-channel panic path.
- **`server::SubscribeError`** — new public enum (`SubscribersPerGroupFull`, `EventGroupsFull`) returned by `SubscriptionManager::subscribe` and `EventPublisher::register_subscriber` when a bounded capacity rejects a subscription. Re-exported from `server::mod`.
- **`Client::new_with_loopback(interface, multicast_loopback)`** — constructor that exposes the previously-internal `multicast_loopback` knob for same-host integration tests.
- **`Client::new_with_spawner_and_loopback(interface, multicast_loopback, spawner)`** — executor-agnostic constructor that accepts a caller-supplied `Spawner` impl. Bare-metal callers swap `TokioSpawner` for their own task pool.
- **`Client::new_with_deps_local`** — constructor for single-threaded / `!Send` executors. Accepts a `LocalSpawner` instead of `Spawner` and relaxes the `Send` bound on the transport socket.
- **`transport::Spawner` trait** (re-exported as `simple_someip::Spawner`) — executor-agnostic task-spawn abstraction. `tokio_transport::TokioSpawner` is the default `std + tokio` impl.
- **`transport::LocalSpawner` trait** — single-threaded task-spawn abstraction for `!Send` futures. Enables use on runtimes like `tokio::LocalSet` or embassy's single-threaded executor.
- **`transport::TransportSocket` / `TransportFactory` / `Timer` traits** — executor-agnostic UDP transport abstraction. Default `tokio_transport::TokioTransport` / `TokioSocket` / `TokioTimer` impls available behind the `client-tokio` / `server-tokio` features.
- **`bare_metal` cargo feature** — activates embassy-sync as the channel backend and enables the `static_channels` module, `AtomicInterfaceHandle`, and `StaticE2EHandle` types. The heap-backed `EmbassySyncChannels` factory is separately gated by the `embassy_channels` feature (which implies `bare_metal`). See `examples/bare_metal_client/` and `examples/bare_metal_server/` for runnable integration examples. Validate with `cargo build -p bare_metal_client` / `cargo build -p bare_metal_server`, NOT `cargo build --workspace` (workspace builds may unify features and mask regressions).
- **`SubscriptionManager::subscribe` returning a `Result`** — see "Changed" below; the regression test list now exercises the major-version mismatch path explicitly.

### Changed

- **Breaking: `Client::new(interface)` return shape** — previously returned `(Client, ClientUpdates)`; now returns `(Client, ClientUpdates, impl Future<Output = ()> + Send + 'static)`. The third element is the run-loop future, which the caller MUST drive (typically via `tokio::spawn`) for any `Client` method to make progress. Migration: change destructuring to a 3-tuple and spawn or otherwise actively poll the future.
- **Breaking: `Server::start_announcing` removed → `Server::announcement_loop`** — the new method returns `Result<impl Future<Output = ()> + Send + 'static, Error>` (annotated `#[must_use]`). Spawn the returned future to fire announcements; calling and dropping the future is a silent no-op.
- **Breaking: `Client::start_sd_announcements` renamed to `Client::sd_announcements_loop`** — same semantic shift as `announcement_loop`: returns an `impl Future` instead of spawning internally, so the caller drives execution.
- **Breaking: `Client::reboot_flag(&self)` now returns `Result<protocol::sd::RebootFlag, Error>`** — previously returned the bare flag and could panic if the run-loop had exited. All other public `Client` methods migrated to the same `Err(Error::Shutdown)` policy in this release; `reboot_flag` is now consistent.
- **Breaking: `server::SubscriptionManager::subscribe` signature change** — now returns `Result<(), server::SubscribeError>` instead of `()`. Previously, capacity rejections were silently dropped with only a `warn!` log, which let the server emit a `SubscribeAck` for a subscription that had not been recorded. Callers must now handle the `Err` path (the server's own SD loop emits `SubscribeNack` on `Err`).
- **Breaking: `server::EventPublisher::register_subscriber` signature change** — now returns `Result<(), server::SubscribeError>` instead of `()`, surfacing the same capacity-rejection signal to externally managed subscription dispatchers.
- **Breaking: `Server::unicast_local_addr` return type changed** — previously returned `Result<std::net::SocketAddr, std::io::Error>`; now returns `Result<std::net::SocketAddr, server::Error>`. Callers that pattern-matched on `std::io::Error` must update to `server::Error::Transport(e)` and access the inner `TransportError` from there.
- **Breaking: default features changed `default = []` → `default = ["std"]`** — previously `embedded-io/std`, `thiserror/std`, and `tracing/std` were always-on; they are now gated behind the new `std` feature. Downstream consumers building with `default-features = false` who relied on the implicit `std` propagation must add `features = ["std"]` (or one of `client` / `server`, which both imply `std`).
- **Breaking: `Client::new` type signature now `Client::<M, R, I, C>::new`** — the `Client` struct gained three additional type parameters for the executor traits (`R: TransportFactory`, `I: InterfaceHandle`, `C: ChannelFactory`). The tokio-default convenience constructor is now gated behind the `client-tokio` feature (was `client`). Migration: add `features = ["client-tokio"]` to continue using `Client::new`; trait-surface consumers use `Client::new_with_deps`.
- **Breaking: `Server::new` type signature now `Server::<R, S, F, Tm>::new`** — the `Server` struct gained type parameters for the pluggable backends. The tokio-default convenience constructor is now gated behind the `server-tokio` feature (was `server`). Migration: add `features = ["server-tokio"]` to continue using `Server::new`; trait-surface consumers use `Server::new_with_deps`.
- **Breaking: `SubscriptionHandle` trait redesigned** — the previous `get_subscribers(&self, …) -> impl Future<Output = Vec<Subscriber>>` method has been replaced with `for_each_subscriber(&self, …, f: FnMut)` visitor pattern. This allows `EventPublisher::publish_event` to copy subscriber addresses into a stack buffer (`heapless::Vec<_, 16>`) instead of allocating per-event. Implementors of custom `SubscriptionHandle` must migrate.
- **Breaking: `SubscriptionHandle` RPITIT futures no longer `+ Send`** — the `subscribe`, `unsubscribe`, and `for_each_subscriber` methods now return `impl Future<…>` without a `+ Send` bound. This enables single-threaded lock-free implementations on bare-metal targets, but means `SubscriptionHandle` trait objects cannot be held across `.await` points in multi-threaded executors. Direct usage with the default `Arc<RwLock<SubscriptionManager>>` is unaffected.
- New optional dependency `dep:futures` (default-features-off) for `futures::select!` + `FusedFuture` plumbing — pulled in transitively by both `client` and `server` features.
- `client::Error::Transport` adopts `#[error(transparent)]` Display delegation (the previous wrapping with `{:?}` debug-formatted the inner `TransportError`); user-facing error strings are now stable.
- Subscribe-NACK reason strings normalized to `snake_case` for log consistency: `wrong_service_id`, `wrong_instance_id`, `wrong_major_version`, `no_endpoint_in_options`, `subscribers_per_group_full`, `event_groups_full`. Wire format is unchanged (NACK is signalled by `TTL=0`).

### Fixed

- **`server::EventPublisher::publish_event` no longer silently sends UNPROTECTED payloads on E2E protect failure** — counter exhaustion / key-lookup races etc. now surface as `Err(Error::E2e(_))` rather than logging and falling through (which had been emitting an unprotected message claiming an E2E-protected channel).
- **SD `Subscribe` with mismatched `major_version` is now NACKed** — previously an Ack would be returned and the subscription registered, leaving the application stack to silently mis-decode incompatible-version traffic.
- **`SocketManager::send` no longer panics on a dropped response oneshot** — user-supplied `Spawner` made this path reachable; failures now return `Err(Error::SocketClosedUnexpectedly)`.
- **`client::Inner` request-queue overflow no longer drops control messages silently** — full queue now invokes `reject_with_capacity("request_queue")` on the rejected message, so callers see a typed `Err(Error::Capacity("request_queue"))` instead of a `RecvError` mapped to `Error::Shutdown`.
- **Per-socket recv-error hot loop bounded** — `SocketManager`'s socket loop now closes after `MAX_CONSECUTIVE_RECV_ERRORS = 16` consecutive `recv_from` failures rather than spinning indefinitely on a permanently broken fd.
- **`Client::send` fails fast on oversize messages** — pre-encode size check returns `Err(Error::Capacity("udp_buffer"))` for messages whose `required_size()` exceeds `UDP_BUFFER_SIZE`. Mirrors the existing `EventPublisher::publish_event` capacity guard.

### Notes

- **Crate version bumped to 0.8.0** — reflects the breaking changes above. Downstream `Cargo.toml` snippets in `README.md` were updated accordingly.

### Test runner

- `tests/client_server.rs` integration tests share the SD multicast port (30490) via `SO_REUSEPORT` and rely on Linux's reuseport hashing for traffic delivery. Under cargo's default parallel test runner cross-test Subscribe deliveries flake. The crate's `.config/nextest.toml` serializes `client_server` via the `serial-sd-port` test-group, so `cargo nextest run` (used by CI) gives stable results. For the legacy harness, pass `--test-threads=1`: `cargo test --test client_server -- --test-threads=1`.


## [0.6.0](https://github.com/luminartech/simple_someip/compare/v0.5.3...v0.6.0) - 2026-04-20

### Other

- Bump to 0.6.0 and fix linting
- Default the reboot flag enum and have it to default to RecentlyRebooted(1) instead of Continuous(0)
- Add loopback support for simple someip.

## [0.5.3](https://github.com/luminartech/simple_someip/compare/v0.5.2...v0.5.3) - 2026-04-15

### Other

- Unify Service Discover across multiple server offers without conflict, HBs flow nicely
- Add a lot of robustness through unit testing and input validation.

## [0.5.2](https://github.com/luminartech/simple_someip/compare/v0.5.1...v0.5.2) - 2026-04-09

### Other

- Update src/client/mod.rs
- Drop the client sender to avoid hanging and delay our first sd message
- Respond to PR Feedback
- More Copilot comments
- Address PR comments - made the sender weak to avoid a hanging reference
- Add an example of how to submit SD messages while a client and server
- Respond to PR feedback and add unit tests.
- Undo server changes and add unit tests.
- Add an explicit command to the client to send SD announcements on a loop
- Allow users to add extra SD entries when sending offers.
- Fix issues sending someip commands on shared ports

## [0.5.1](https://github.com/luminartech/simple_someip/compare/v0.5.0...v0.5.1) - 2026-04-03

### Other

- Automatically create semver appropriate release PR
- Fix test.
- Respond to Copilot feedback
- Add a "Subscribe No Wait" to avoid blocking on subscriptions, + tests
- Formatted & remove duplicate sd payload.
- Tie SD session IDs to per service instances
- Pacify Clippy.
- Fix false reboot detection with interleaved SD session IDs

## [0.5.0] - 2026-03-12

### Breaking Changes

- **Split `Client` into handle + update stream** — `Client::new()` now returns
  `(Client, ClientUpdates)` instead of `Self`. The `Client` handle is `Clone`-able
  and all methods take `&self`, allowing concurrent use from multiple tasks without
  `Arc<Mutex<_>>`. `ClientUpdates::recv()` replaces the old `client.run()` method.
- **`shut_down()` is no longer async** — `Client::shut_down(self)` drops the control
  channel synchronously. The inner event loop exits once all `Client` clones are dropped.
- **`add_endpoint` takes a `local_port` parameter** — controls the source port used
  when sending to the endpoint. Pass `0` for an ephemeral OS-assigned port.

### Added

- **`Client::request()`** — send a message and await the response in one call, without
  needing to drive `ClientUpdates::recv()` concurrently.
- **`Client::send_to_service()`** — returns a `PendingResponse` handle for manual
  request-response control.
- **Multiple concurrent requests** — the inner event loop now tracks pending responses
  in a `HashMap` keyed by `request_id`, supporting multiple in-flight request-response
  transactions.
- **Automatic E2E management** — `Client::register_e2e()` / `unregister_e2e()` and
  `Server::register_e2e()` / `unregister_e2e()` configure End-to-End protection per
  message key. Incoming messages are checked and outgoing messages are protected
  automatically.
- **`EventPublisher::publish_event()`** — type-safe event publishing using `Message<P>`
  instead of raw bytes.
- **`EventPublisher::subscriber_count()`** — query the number of subscribers for an
  event group.

### Fixed

- **SD spec compliance** — `SubscribeAck` and `SubscribeNack` are now sent from the SD
  socket (port 30490) instead of the unicast socket, matching the SOME/IP-SD specification
  requirement that all SD messages originate from the SD port.

## [0.4.0] - 2026-03-04

### Breaking Changes

- **Zero-copy parsing** — `Header::read_from_bytes` / `Message::read_from_bytes` replaced by
  `HeaderView::parse` and `MessageView::parse`, which return borrowed views instead of owned
  structs. SD headers follow the same pattern with `SdHeaderView::parse`.
- **Simplified error types** — flattened and consolidated error enums across the crate.
- **Encapsulated protocol header** — `Header` fields are no longer public; use constructors and
  accessors instead.
- **Removed `send_message` / binding API** — the client now manages socket binding internally;
  `Client::add_endpoint` / `Client::remove_endpoint` replace the old approach.
- **Re-exported traits at crate root** — `WireFormat` and `PayloadWireFormat`
  are now available directly from `simple_someip::*`.

### Added

- **Service registry** — `Client::add_endpoint` / `Client::remove_endpoint` and
  `Client::send_to_service` for programmatic endpoint management.
- **Session handling** — the client now tracks SD session IDs per sender and detects reboots
  via `ClientUpdate::SenderRebooted`.
- **Comprehensive API documentation** — doc comments with `# Errors` and `# Panics` sections
  on every public function; crate-level rustdoc with usage examples.

### Changed

- SD constants moved into the `protocol::sd` module.
- Standalone discovery example with proper feature-gated dependencies.

## [0.3.0] - 2026-02-25

Initial public release.
