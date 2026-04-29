#!/bin/sh
# Templates VSOMEIP_UNICAST into the role-specific JSON and execs
# the chosen vsomeip role. Two roles:
#   VSOMEIP_ROLE=offerer    (default) — advertises service 0x1234
#   VSOMEIP_ROLE=subscriber           — requests + watches for it
#
# VSOMEIP_UNICAST is required (vsomeip 3.4.10 doesn't honor any
# unicast-override env var directly, and `unicast: 127.0.0.1`
# doesn't work on Linux — lo lacks the MULTICAST flag, so SD
# multicast never reaches the wire). Pick the IP of an actual
# multicast-capable interface on the host:
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

ROLE="${VSOMEIP_ROLE:-offerer}"
case "${ROLE}" in
    offerer)
        SRC_JSON=/etc/vsomeip-offerer.json
        DST_JSON=/tmp/vsomeip-offerer.json
        BINARY=/usr/local/bin/offerer
        APP_NAME=offerer
        ;;
    subscriber)
        SRC_JSON=/etc/vsomeip-subscriber.json
        DST_JSON=/tmp/vsomeip-subscriber.json
        BINARY=/usr/local/bin/subscriber
        APP_NAME=subscriber
        ;;
    *)
        echo "ERROR: VSOMEIP_ROLE='${ROLE}' invalid; use 'offerer' or 'subscriber'." 1>&2
        exit 1
        ;;
esac

# Templated config goes to /tmp because /etc is read-only-ish from
# the image's COPY layer.
sed "s/VSOMEIP_UNICAST_PLACEHOLDER/${VSOMEIP_UNICAST}/" \
    "${SRC_JSON}" > "${DST_JSON}"

export VSOMEIP_CONFIGURATION="${DST_JSON}"
export VSOMEIP_APPLICATION_NAME="${APP_NAME}"

exec "${BINARY}"
