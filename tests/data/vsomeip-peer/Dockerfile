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
# deployments simple-someip interoperates with. CI's nightly run also
# builds it against the newest upstream release, which needs a newer Boost
# than Ubuntu 22.04 ships (vsomeip 3.7 wants >= 1.75):
#   --build-arg VSOMEIP_VERSION=3.7.6 --build-arg BASE_IMAGE=ubuntu:24.04 \
#   --build-arg BOOST_VERSION=1.83.0
ARG BASE_IMAGE=ubuntu:22.04

FROM ${BASE_IMAGE} AS build

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

ARG VSOMEIP_VERSION=3.4.10

WORKDIR /src

# Pull the vsomeip release from upstream.
RUN wget -q "https://github.com/COVESA/vsomeip/archive/refs/tags/${VSOMEIP_VERSION}.tar.gz" \
    && tar xzf "${VSOMEIP_VERSION}.tar.gz" \
    && rm "${VSOMEIP_VERSION}.tar.gz"

WORKDIR /src/vsomeip-${VSOMEIP_VERSION}

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
FROM ${BASE_IMAGE}

ARG BOOST_VERSION=1.74.0
ARG VSOMEIP_VERSION=3.4.10

LABEL org.opencontainers.image.source="https://github.com/luminartech/simple_someip" \
      org.opencontainers.image.licenses="(MIT OR Apache-2.0) AND MPL-2.0 AND BSL-1.0" \
      org.opencontainers.image.description="vsomeip peer driven by stdin commands, for simple-someip interop tests" \
      com.github.luminartech.simple-someip.vsomeip.version="${VSOMEIP_VERSION}" \
      com.github.luminartech.simple-someip.vsomeip.source="https://github.com/COVESA/vsomeip/tree/${VSOMEIP_VERSION}"

RUN apt-get update && apt-get install -y --no-install-recommends \
        "libboost-system${BOOST_VERSION}" \
        "libboost-filesystem${BOOST_VERSION}" \
        "libboost-thread${BOOST_VERSION}" \
        "libboost-log${BOOST_VERSION}" \
    && rm -rf /var/lib/apt/lists/*

# Copy installed vsomeip libs, the peer, its config template and the entrypoint.
COPY --from=build /usr/local/lib/libvsomeip3*.so* /usr/local/lib/
COPY --from=build /src/peer/build/peer /usr/local/bin/peer
# vsomeip is MPL-2.0: ship its license and say where its source lives.
COPY --from=build /src/vsomeip-${VSOMEIP_VERSION}/LICENSE /usr/share/doc/vsomeip/LICENSE
RUN printf '%s\n' \
        "vsomeip ${VSOMEIP_VERSION}" \
        "License: MPL-2.0 (see LICENSE in this directory)" \
        "Source: https://github.com/COVESA/vsomeip/tree/${VSOMEIP_VERSION}" \
        "Built unmodified from the upstream release tarball." \
        > /usr/share/doc/vsomeip/SOURCE
COPY peer.json.in /etc/vsomeip-peer.json.in
COPY entrypoint.sh /usr/local/bin/entrypoint.sh

RUN ldconfig && chmod +x /usr/local/bin/entrypoint.sh

# The entrypoint templates VSOMEIP_UNICAST into the peer's config and
# execs the peer. Pass `-e VSOMEIP_UNICAST=<address>` on `docker run`.
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
