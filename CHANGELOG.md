# Changelog

## [Unreleased]

### Added

- **`client::Error::Capacity(&'static str)`** — new variant returned when a fixed-capacity internal structure is full (e.g. `"unicast_sockets"`, `"udp_buffer"`). Because `client::Error` is not `#[non_exhaustive]`, this is a breaking change for downstream crates that match the enum exhaustively.
- **`server::SubscribeError`** — new public enum (`SubscribersPerGroupFull`, `EventGroupsFull`) returned by `SubscriptionManager::subscribe` and `EventPublisher::register_subscriber` when a bounded capacity rejects a subscription. Re-exported from `server::mod`.

### Changed

- **`std` is now the default feature** — the crate enables `std` (with `thiserror` and `tracing`) by default. Users targeting `no_std` environments must set `default-features = false` in their `Cargo.toml`.
- **`thiserror` and `tracing` use `default-features = false`** — both dependencies are always included but their `std` features are only enabled when the crate's `std` feature is active. This removes the need for `#[cfg(feature = "std")]` gating on error types and logging macros.
- **Breaking: `server::SubscriptionManager::subscribe` signature change** — now returns `Result<(), server::SubscribeError>` instead of `()`. Previously, capacity rejections were silently dropped with only a `warn!` log, which let the server emit a `SubscribeAck` for a subscription that had not been recorded. Callers must now handle the `Err` path (the server's own SD loop emits `SubscribeNack` on `Err`).
- **Breaking: `server::EventPublisher::register_subscriber` signature change** — now returns `Result<(), server::SubscribeError>` instead of `()`, surfacing the same capacity-rejection signal to externally managed subscription dispatchers.


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
