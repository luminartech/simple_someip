# Contributing

Pull requests are welcome, as are bug reports and questions in the issue
tracker.

The crate implements the
[Open SOME/IP Specification](https://github.com/some-ip-com/open-someip-spec).
`README.md` describes the module layout; this document covers the parts of the
build that are easy to get wrong.

## The feature graph

`default = ["std"]`, and the feature set is the most intricate thing about this
crate. The client and server each split into a trait surface and a tokio
convenience layer:

- `client` / `server` — the executor-agnostic trait surface. No tokio, no
  socket2. Callers supply their own `Spawner`, `Timer`, `ChannelFactory` and
  `TransportFactory`.
- `client-tokio` / `server-tokio` — the same, plus the tokio defaults that make
  `Client::new` and `Server::new` work. Both force `std`.
- `bare_metal` — activates embassy-sync as the channel backend along with
  `static_channels`, `AtomicInterfaceHandle` and `StaticE2EHandle`. Enabling it
  alone does **not** make the crate bare-metal-complete: `client` and `server`
  still need user-provided `Spawner` and `TransportFactory` impls.
- `bare-metal-runtime` — the composed embassy runtime on top of that.
- `embassy_channels` — heap-backed channel backend, useful before static pools
  are sized.
- `_alloc` — private marker for "this build needs `extern crate alloc`". It is
  tied to the declaration in `lib.rs` so both sides move together. Do not
  enable it directly.

`tracing` is feature-gated rather than always-on because `tracing-core` declares
`extern crate alloc` unconditionally, which bare-metal targets shipping a
`core`-only sysroot cannot satisfy.

## Testing

Most integration tests declare `required-features`, so a bare `cargo test`
silently skips them. Run the configurations, not just the default:

```sh
cargo test --features client-tokio,server-tokio       # the async engines
cargo test --features client,bare_metal               # the bare-metal client
cargo test --features server,bare_metal               # the bare-metal server
cargo test --all-features
```

Two tests are allocation witnesses (`no_alloc_witness`,
`no_alloc_server_witness`) and run with `harness = false`: they fail if the
no-alloc paths start allocating. Treat a change that trips one as a design
question, not a test to adjust.

The bare-metal examples are workspace members and exercise the real
configuration rather than a feature flag on the root crate:

```sh
cargo build -p bare_metal_client
cargo build -p bare_metal_server
```

`tests/wire_golden.rs` pins the bytes on the wire. A round-trip test written
against the crate's own output passes whenever encode and decode share the same
misreading, so a wire-format change should either fail a golden vector or add
one.

## Minimum supported Rust version

The manifest deliberately declares no `rust-version`, and CI's MSRV job is
switched off as a result. Picking a floor is a policy decision about what the
project commits to supporting — if you need one, raise it as an issue rather
than adding it as a side effect of another change.

## Commits and pull requests

Commit subjects follow [Conventional Commits](https://www.conventionalcommits.org/)
(`feat:`, `fix:`, `docs:`, `build:`, `chore:`, with a `!` for a breaking
change), because the changelog is organized around them.

`Linear PR History` is a required check: rebase onto `main` rather than merging
`main` into your branch.

## Continuous integration

Two pipelines currently run side by side: this repository's own workflow, which
produces `Format & Lint`, `Build, Test & Coverage`, the Windows and
bare-metal/no_std builds and `SemVer Check`; and the org-wide reusable workflow
from [`luminartech/rust_workflow`](https://github.com/luminartech/rust_workflow),
which produces the `ci / *` checks. The three required checks come from the
former.

## Releases

[release-plz](https://release-plz.dev) owns versioning, the changelog, tags,
GitHub releases, and the crates.io publish. There is no version to bump by
hand: a push to `main` maintains an open release PR, and merging that PR
publishes.
