# Changelog

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
