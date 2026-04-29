# vsomeip 3.4.10 + a minimal offerer that advertises service 0x1234
# instance 0x0001 via SD multicast. Used by phase 20f's host-side
# conformance test (`tests/vsomeip_sd_compat.rs`).
#
# Build:
#   docker build -t vsomeip-offerer tests/data/vsomeip-offerer/
#
# Run (host network mode so SD multicast 224.0.23.0:30490 reaches the
# host's listener — required for the cargo test to receive the
# OfferService broadcast):
#   docker run --rm -d --name vsomeip-offerer --network host \
#       vsomeip-offerer
#
# Verify it's emitting:
#   docker logs vsomeip-offerer
#
# Stop:
#   docker stop vsomeip-offerer
#
# Pinning to vsomeip 3.4.10 specifically because that's the version
# LumPDK / EnVision use (see LumPDK/packages/thirdparty/vsomeip/
# vsomeip.MODULE.bazel). Keeping wire-version-aligned with production
# avoids interop quirks during CI conformance testing.

FROM ubuntu:22.04 AS build

# vsomeip's CMake build needs: gcc, cmake, boost, and a few utilities
# for the patch-and-build flow. dlt is optional and we skip it.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        cmake \
        git \
        ca-certificates \
        wget \
        libboost-system-dev \
        libboost-filesystem-dev \
        libboost-thread-dev \
        libboost-log-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Pull vsomeip 3.4.10 from upstream.
RUN wget -q https://github.com/COVESA/vsomeip/archive/refs/tags/3.4.10.tar.gz \
    && tar xzf 3.4.10.tar.gz \
    && rm 3.4.10.tar.gz

WORKDIR /src/vsomeip-3.4.10

# Build vsomeip as shared libs (default). Skip building the test/
# example trees — we'll compile our own offerer against the installed
# library.
RUN mkdir build && cd build \
    && cmake .. \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_INSTALL_PREFIX=/usr/local \
    && make -j"$(nproc)" \
    && make install \
    && ldconfig

# Build our offerer. It links against the just-installed libvsomeip3.
COPY offerer.cpp /src/offerer/offerer.cpp
COPY CMakeLists.txt /src/offerer/CMakeLists.txt

WORKDIR /src/offerer
RUN mkdir build && cd build \
    && cmake .. \
        -DCMAKE_BUILD_TYPE=Release \
    && make -j"$(nproc)"

# ── Runtime image ────────────────────────────────────────────────────
FROM ubuntu:22.04

RUN apt-get update && apt-get install -y --no-install-recommends \
        libboost-system1.74.0 \
        libboost-filesystem1.74.0 \
        libboost-thread1.74.0 \
        libboost-log1.74.0 \
    && rm -rf /var/lib/apt/lists/*

# Copy installed vsomeip libs + the offerer binary + the config + entrypoint.
COPY --from=build /usr/local/lib/libvsomeip3*.so* /usr/local/lib/
COPY --from=build /src/offerer/build/offerer /usr/local/bin/offerer
COPY offerer.json /etc/vsomeip-offerer.json
COPY entrypoint.sh /usr/local/bin/entrypoint.sh

RUN ldconfig && chmod +x /usr/local/bin/entrypoint.sh

# Entrypoint script templates VSOMEIP_UNICAST into the JSON config
# before launching the offerer. Caller MUST pass `-e VSOMEIP_UNICAST=
# <multicast-capable-iface-IP>` on `docker run`; the script exits
# loudly otherwise. See entrypoint.sh for the rationale (lo's lack of
# MULTICAST flag is the gotcha).
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
