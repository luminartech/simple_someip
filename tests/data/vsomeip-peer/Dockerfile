# vsomeip 3.4.10 + a peer program driven by commands on stdin, used by
# simple-someip's interop tests. See README.md for the command protocol.
#
# Build:
#   docker build --network=host -t simple-someip-vsomeip-peer tests/data/vsomeip-peer/
#
# Run (host network mode so SD multicast reaches the stack under test):
#   docker run -i --rm --network host -e VSOMEIP_UNICAST=127.0.0.2 \
#       simple-someip-vsomeip-peer
#
# Pinned to vsomeip 3.4.10 to stay wire-version-aligned with the
# deployments simple-someip interoperates with.

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
# example trees — we'll compile our own peer against the installed
# library.
RUN mkdir build && cd build \
    && cmake .. \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_INSTALL_PREFIX=/usr/local \
    && make -j"$(nproc)" \
    && make install \
    && ldconfig

# Build the peer. It links against the just-installed libvsomeip3.
COPY peer.cpp       /src/peer/peer.cpp
COPY CMakeLists.txt /src/peer/CMakeLists.txt
WORKDIR /src/peer
RUN mkdir build && cd build \
    && cmake .. -DCMAKE_BUILD_TYPE=Release \
    && make -j"$(nproc)"

# ── Runtime image ────────────────────────────────────────────────────
FROM ubuntu:22.04

RUN apt-get update && apt-get install -y --no-install-recommends \
        libboost-system1.74.0 \
        libboost-filesystem1.74.0 \
        libboost-thread1.74.0 \
        libboost-log1.74.0 \
    && rm -rf /var/lib/apt/lists/*

# Copy installed vsomeip libs, the peer, its config template and the entrypoint.
COPY --from=build /usr/local/lib/libvsomeip3*.so* /usr/local/lib/
COPY --from=build /src/peer/build/peer /usr/local/bin/peer
COPY peer.json.in /etc/vsomeip-peer.json.in
COPY entrypoint.sh /usr/local/bin/entrypoint.sh

RUN ldconfig && chmod +x /usr/local/bin/entrypoint.sh

# The entrypoint templates VSOMEIP_UNICAST into the peer's config and
# execs the peer. Pass `-e VSOMEIP_UNICAST=<address>` on `docker run`.
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
