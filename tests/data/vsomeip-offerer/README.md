# vsomeip offerer for phase-20f conformance testing

A Docker image that builds vsomeip 3.4.10 (the version LumPDK /
EnVision pin) and runs a tiny C++ offerer advertising service
`0x1234` instance `0x0001` via SOME/IP-SD. The companion
`tests/vsomeip_sd_compat.rs` test on the host listens for that
broadcast.

## Build

```sh
docker build -t vsomeip-offerer tests/data/vsomeip-offerer/
```

First build pulls vsomeip from upstream and compiles it in the
container — expect 5–10 minutes on a typical workstation.
Subsequent builds use Docker's layer cache.

## Run

First, find a multicast-capable interface IP on your host:

```sh
ip route get 224.0.23.0
# Expected output:
#   multicast 224.0.23.0 dev wlp0s20f3 src 192.168.1.42 uid 1000
#                                          ^^^^^^^^^^^^
# That last IP is what you pass below. Lo (127.0.0.1) does NOT
# work — Linux's loopback interface lacks the MULTICAST flag by
# default, so SD multicast never leaves the host.
```

Then launch the offerer:

```sh
docker run --rm -d --name vsomeip-offerer --network host \
    -e VSOMEIP_UNICAST=192.168.1.42 \
    vsomeip-offerer
```

`--network host` is required so SD multicast (`224.0.23.0:30490`)
flows on the actual host interface. The `VSOMEIP_UNICAST` env var
gets templated into the JSON config at container start by
`entrypoint.sh`.

Verify it's up:

```sh
docker logs vsomeip-offerer
# Expected (debug level): "Joining to multicast group 224.0.23.0 from <your IP>"
# and "OFFER(1277): [1234.0001:1.0] (true)"
```

## Test against it

In another terminal:

```sh
SIMPLE_SOMEIP_TEST_INTERFACE=192.168.1.42 \
    cargo test --features client-tokio,server-tokio \
        --test vsomeip_sd_compat -- --ignored --nocapture
```

Use the **same IP** you passed via `VSOMEIP_UNICAST`. Expected:
`client_sees_vsomeip_offer_service ... ok` in well under a second
once vsomeip's first SD broadcast fires (~100 ms after offer
registration, then every 1 s thereafter).

## Stop

```sh
docker stop vsomeip-offerer
```

## Files

- `Dockerfile` — multi-stage: builds vsomeip + the offerer in stage 1,
  copies the runtime artifacts into a slim runtime stage.
- `offerer.cpp` — ~50 LOC vsomeip-based offerer; calls
  `application->offer_service(0x1234, 0x0001, 1, 0)` and idles
  while vsomeip emits SD broadcasts.
- `CMakeLists.txt` — builds `offerer` against installed `libvsomeip3`.
- `offerer.json` — vsomeip configuration. `unicast` is templated
  via `VSOMEIP_UNICAST` env var at container start (see
  `entrypoint.sh`). Standard SD multicast `224.0.23.0:30490`.
- `entrypoint.sh` — substitutes `VSOMEIP_UNICAST` into the JSON
  config before launching the offerer; bails loudly if the env
  var isn't set.

## Why these specific values

- vsomeip 3.4.10: matches `LumPDK/packages/thirdparty/vsomeip/vsomeip.MODULE.bazel`
  so CI conformance tests run against the same wire-version
  production validation does.
- Service `0x1234` instance `0x0001`: hardcoded in both this
  config and `tests/vsomeip_sd_compat.rs`. Change one, change the
  other.
- Multicast `224.0.23.0:30490`: SOME/IP-SD spec default. (LumPDK's
  production config uses `239.255.0.5:30491` but that's a
  Luminar-network-specific choice; for the host-side conformance
  test, sticking to spec defaults removes a configuration knob.)
- `unicast: "127.0.0.1"`: works under Docker host-network mode
  because the host and container share the loopback interface.
  For real-NIC testing, set this to the host's interface IP and
  set `SIMPLE_SOMEIP_TEST_INTERFACE` to match.

## Future (phase 20g+)

- Wire this Dockerfile into CI via TestContainers-rs (or
  equivalent) so `cargo test ... -- --ignored` runs in a
  CI runner with Docker available.
- Apply LumPDK's vsomeip patches to the build (especially the
  E2E Profile 5 patch) once we add E2E-conformance tests.
