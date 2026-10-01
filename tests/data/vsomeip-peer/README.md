# vsomeip interop peer

A small [vsomeip](https://github.com/COVESA/vsomeip) 3.4.10 application used by
simple-someip's interop tests (`tests/interop_std.rs`, `tests/interop_bare_metal.rs`).
The tests start it in a container, send it commands on stdin, and read what it
observes from stdout.

## Running the interop tests locally

Requirements: Docker, cargo-nextest, loopback multicast, and the peer's
address (127.0.0.2) assigned to the loopback interface:

    sudo ip link set lo multicast on
    sudo ip route replace 239.255.0.255/32 dev lo
    sudo ip addr replace 127.0.0.2/8 dev lo

Build the image once (5–10 minutes):

    docker build --network=host -t simple-someip-vsomeip-peer tests/data/vsomeip-peer/

Run the std runtime's interop tests:

    cargo nextest run --profile interop --features std,tracing,client,client-tokio,server,server-tokio --test interop_std

Run the bare-metal runtime's interop tests (nightly toolchain):

    cargo +nightly-2026-08-21 nextest run --profile interop --no-default-features --features bare-metal-runtime --test interop_bare_metal

Add `--run-ignored only` to see which known gaps are still open; each ignored
test names the issue that tracks it.

## Licenses and source

The image contains vsomeip (MPL-2.0) and Boost (BSL-1.0). vsomeip's license and
a pointer to its source are inside the image at `/usr/share/doc/vsomeip/LICENSE`
and `/usr/share/doc/vsomeip/SOURCE`. It is built unmodified, so the source for
the version in use is the matching upstream tag at
`https://github.com/COVESA/vsomeip/tree/<version>`.

## Command protocol

One command per line on stdin; ids are hex without `0x`, payloads are hex, `-`
means empty.

| Command | Effect |
|---|---|
| `offer <svc> <inst> <major> <eg> <events> <fields> <methods>` | Offer a service with the given events and fields (comma-separated or `-`). The method list is checked for well-formed ids but not otherwise used: the peer answers a request for any method |
| `stop-offer <svc> <inst>` | Stop offering |
| `notify <svc> <inst> <event> <payload>` / `set-field …` | Send an event / set a field value |
| `require <svc> <inst> <major>` | Look for a service |
| `subscribe <svc> <inst> <eg> <major> <events> <fields>` | Subscribe to an eventgroup |
| `unsubscribe <svc> <inst> <eg>` | Unsubscribe |
| `call <svc> <inst> <method> <payload>` / `fire …` | Send a request / a fire-and-forget request |
| `error-reply <method> <return_code>` | Answer future requests for `method` with an error |
| `quit` | Stop |

Output lines: `READY`, `AVAILABLE`, `UNAVAILABLE`, `SUBSCRIPTION`, `EVENT`,
`REQUEST`, `RESPONSE`, `OK <command>`, `ERR <message>`.
