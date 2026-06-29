# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/luminartech/simple_someip/releases/tag/v0.1.0) - 2026-06-29

### Added

- *(server)* thread recv+announce send-scratch through run_with_buffers; run future 7696->3528 B (#125 PR3 T2+T3)
- *(client)* move socket-loop buffer out of the future via BufferProvider; oversize drop/reject ([#125](https://github.com/luminartech/simple_someip/pull/125))

### Fixed

- *(lint,docs)* nightly clippy, intra-doc links, + rustfmt on the consolidated branch
- *(client)* make inbound-oversize drop+survive real on embassy-net; doc + test cleanup ([#125](https://github.com/luminartech/simple_someip/pull/125))

### Other

- *(embassy-net)* add missing non_sd_observer field to ServerDeps inits
- rustfmt + clippy cleanup over the #124 base
- phase 21: collapse [Unreleased] into [0.8.0]
- phase 21b + 21F: Server constructor reshape + SubscriptionHandle GATs
- phase 21 cleanup: address adversarial review
- phase 20 cleanup: correct embassy-sync dep-version comment
- phase 20 cleanup: tests for new code + Copilot review fixes
- phase 20 cleanup: changelog [Unreleased] + final verification
- phase 20 cleanup: MED clusters A/B/C/D
- phase 20 cleanup: workspace clippy + embassy-net adapter soundness
- phase 19g: SOME/IP Client+Server roundtrip over embassy-net loopback
- phase 19e: adapter-level loopback test (LoopbackDriver pair + UDP roundtrip)
- phase 19c: EmbassyNetSocket send/recv via poll_send_to / poll_recv_from
- phase 19b: EmbassyNetFactory + SocketPool storage
- phase 19a: scaffold simple-someip-embassy-net workspace member
