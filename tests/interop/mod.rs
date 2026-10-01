//! Shared interop test code, compiled into both `interop_std` and
//! `interop_bare_metal`.
#![allow(dead_code)] // each target uses a different subset

pub mod peers;
pub mod runtime;
pub mod runtimes;
pub mod scenarios;

use std::process::Command;

pub mod consts {
    use std::net::Ipv4Addr;
    pub const SVC: u16 = 0x1234;
    pub const INST: u16 = 0x0001;
    pub const MAJOR: u8 = 1;
    pub const EG: u16 = 0x0001;
    pub const EVENT: u16 = 0x8001;
    pub const FIELD: u16 = 0x8002;
    pub const METHOD: u16 = 0x0001;
    pub const SERVER_PORT: u16 = 30509;
    pub const CLIENT_PORT: u16 = 30510;
    pub const SD_PORT: u16 = 30490;
    pub const SD_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 0, 255);
    pub const OUR_IP: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);
    pub const PEER_IP: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 2);
}

const LOOPBACK_SETUP: &str = "run:\n  \
    sudo ip link set lo multicast on\n  \
    sudo ip route replace 239.255.0.255/32 dev lo\n  \
    sudo ip addr add 127.0.0.2/8 dev lo";

/// Fails fast, with the commands to run, when the host isn't set up for
/// loopback multicast or the peer's address isn't assigned to `lo`.
pub fn preflight() {
    let link = ip(&["link", "show", "lo"]);
    let route = ip(&["route", "get", "239.255.0.255"]);
    let addrs = ip(&["-4", "addr", "show", "dev", "lo"]);
    if let Err(msg) = check_loopback(&link, &route, &addrs) {
        panic!("{msg}");
    }
}

fn ip(args: &[&str]) -> String {
    let out = Command::new("ip")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("could not run `ip {}` ({e})", args.join(" ")));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Checks the output of `ip link show lo`, `ip route get 239.255.0.255` and
/// `ip -4 addr show dev lo`.
pub fn check_loopback(link: &str, route: &str, addrs: &str) -> Result<(), String> {
    let mut problems = Vec::new();

    let multicast = link
        .split_once('<')
        .and_then(|(_, rest)| rest.split_once('>'))
        .is_some_and(|(flags, _)| flags.split(',').any(|f| f == "MULTICAST"));
    if !multicast {
        problems.push("multicast is not enabled on lo");
    }

    let words: Vec<&str> = route.split_whitespace().collect();
    let via_lo = words.windows(2).any(|w| w == ["dev", "lo"]);
    if !via_lo {
        problems.push("239.255.0.255 is not routed via lo");
    }

    let words: Vec<&str> = addrs.split_whitespace().collect();
    let has_peer_ip = words
        .windows(2)
        .any(|w| w[0] == "inet" && w[1].split('/').next() == Some("127.0.0.2"));
    if !has_peer_ip {
        problems.push("127.0.0.2 is not assigned to lo");
    }

    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "loopback is not set up for the interop tests ({}); {LOOPBACK_SETUP}",
            problems.join(", ")
        ))
    }
}
