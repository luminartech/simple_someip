#!/bin/sh
# Starts the vsomeip interop peer. VSOMEIP_UNICAST is the address the peer
# uses; for loopback testing use 127.0.0.2 so it differs from the address
# of the stack under test.
set -eu
if [ -z "${VSOMEIP_UNICAST:-}" ]; then
    echo "ERROR: set VSOMEIP_UNICAST, e.g. docker run -e VSOMEIP_UNICAST=127.0.0.2 ..." 1>&2
    exit 1
fi
sed "s/VSOMEIP_UNICAST_PLACEHOLDER/${VSOMEIP_UNICAST}/" /etc/vsomeip-peer.json.in > /tmp/vsomeip-peer.json
export VSOMEIP_CONFIGURATION=/tmp/vsomeip-peer.json
export VSOMEIP_APPLICATION_NAME=peer
exec /usr/local/bin/peer
