# Changelog

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
- **Re-exported traits at crate root** — `WireFormat`, `PayloadWireFormat`, and
  `DiscoveryOnlyPayload` are now available directly from `simple_someip::*`.

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
