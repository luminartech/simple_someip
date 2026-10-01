//! Interop tests for the std runtime against vsomeip and spec-derived frames.
#![cfg(all(feature = "client-tokio", feature = "server-tokio"))]

mod interop;

use std::time::Duration;

use interop::peers::vsomeip::VsomeipPeer;

#[test]
fn peer_starts_and_accepts_commands() {
    interop::preflight();
    let mut peer = VsomeipPeer::start();
    peer.send("offer 1234 0001 1 0001 8001 8002 0001");
    peer.expect_none("ERR", Duration::from_millis(200));
}

mod loopback_check {
    use super::interop::check_loopback;

    const LINK_OK: &str = "1: lo: <LOOPBACK,MULTICAST,UP,LOWER_UP> mtu 65536 qdisc noqueue \
        state UNKNOWN mode DEFAULT group default qlen 1000\n    \
        link/loopback 00:00:00:00:00:00 brd 00:00:00:00:00:00\n";
    const ROUTE_OK: &str = "multicast 239.255.0.255 dev lo src 192.168.5.166 uid 1001 \n    \
        cache <mc> \n";
    const ADDRS_OK: &str = "1: lo: <LOOPBACK,MULTICAST,UP,LOWER_UP> mtu 65536 qdisc noqueue \
        state UNKNOWN group default qlen 1000\n    \
        inet 127.0.0.1/8 scope host lo\n       valid_lft forever preferred_lft forever\n    \
        inet 127.0.0.2/8 scope host secondary lo\n       valid_lft forever preferred_lft forever\n";

    #[test]
    fn accepts_a_configured_host() {
        assert_eq!(check_loopback(LINK_OK, ROUTE_OK, ADDRS_OK), Ok(()));
    }

    #[test]
    fn rejects_lo_without_multicast() {
        let link = LINK_OK.replace("MULTICAST,", "");
        let err = check_loopback(&link, ROUTE_OK, ADDRS_OK).unwrap_err();
        assert!(err.contains("multicast is not enabled on lo"), "{err}");
        assert!(err.contains("sudo ip link set lo multicast on"), "{err}");
    }

    #[test]
    fn rejects_a_route_via_another_device() {
        let route = "multicast 239.255.0.255 dev gpd0 src 10.0.22.22 uid 1001 \n    cache <mc> \n";
        let err = check_loopback(LINK_OK, route, ADDRS_OK).unwrap_err();
        assert!(err.contains("239.255.0.255 is not routed via lo"), "{err}");
        assert!(
            err.contains("sudo ip route replace 239.255.0.255/32 dev lo"),
            "{err}"
        );
    }

    #[test]
    fn rejects_lo_without_the_peer_address() {
        let addrs = ADDRS_OK
            .lines()
            .take(3)
            .map(|l| format!("{l}\n"))
            .collect::<String>();
        assert!(!addrs.contains("127.0.0.2"));
        let err = check_loopback(LINK_OK, ROUTE_OK, &addrs).unwrap_err();
        assert!(err.contains("127.0.0.2 is not assigned to lo"), "{err}");
        assert!(err.contains("sudo ip addr add 127.0.0.2/8 dev lo"), "{err}");
    }

    #[test]
    fn lists_every_setup_command_for_any_failure() {
        let err = check_loopback("", "", "").unwrap_err();
        for cmd in [
            "sudo ip link set lo multicast on",
            "sudo ip route replace 239.255.0.255/32 dev lo",
            "sudo ip addr add 127.0.0.2/8 dev lo",
        ] {
            assert!(err.contains(cmd), "{err}");
        }
    }
}
