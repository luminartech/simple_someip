#!/bin/sh
# Templates VSOMEIP_UNICAST into /etc/vsomeip-offerer.json then exec
# the offerer. The env var MUST be set on docker run; vsomeip 3.4.10
# does not honor a VSOMEIP_UNICAST_ADDRESS-style env var directly,
# and `unicast: 127.0.0.1` doesn't work on Linux (lo lacks the
# MULTICAST flag, so SD multicast never reaches the wire). Pick the
# IP of an actual multicast-capable interface on the host:
#
#   ip route get 224.0.23.0
#
# returns "multicast 224.0.23.0 dev <iface> src <IP> ..." — use
# that <IP>.

set -eu

if [ -z "${VSOMEIP_UNICAST:-}" ]; then
    echo "ERROR: set VSOMEIP_UNICAST=<iface-IP> on docker run."         1>&2
    echo "       e.g. 'docker run -e VSOMEIP_UNICAST=192.168.1.10 ...'" 1>&2
    echo "       Find your interface IP via 'ip route get 224.0.23.0'." 1>&2
    exit 1
fi

# Templated config goes to a writable location since /etc/ in the
# image is read-only-ish from the build's COPY.
sed "s/VSOMEIP_UNICAST_PLACEHOLDER/${VSOMEIP_UNICAST}/" \
    /etc/vsomeip-offerer.json > /tmp/vsomeip-offerer.json

export VSOMEIP_CONFIGURATION=/tmp/vsomeip-offerer.json
export VSOMEIP_APPLICATION_NAME=offerer

exec /usr/local/bin/offerer
