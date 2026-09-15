# Security policy

## Reporting a vulnerability

Report security issues through GitHub's private vulnerability reporting: open
the [Security tab](https://github.com/luminartech/simple_someip/security) and
choose **Report a vulnerability**. That opens a private advisory visible only
to the maintainers.

Please do not open a public issue for a security report.

A report is most useful with the crate version, the feature set enabled, and a
byte sequence or test case that reproduces the behavior.

## Supported versions

This crate is pre-1.0. Fixes land on the latest published version; there are no
maintained release branches.

## Scope

`simple-someip` implements the SOME/IP wire format, service discovery, and the
client and server engines above them. Three properties of SOME/IP matter when
assessing a report, because they are the protocol's design rather than defects
in this crate:

- **SOME/IP carries no transport security.** Messages travel as plaintext UDP
  or TCP. There is no confidentiality, and nothing in the protocol binds a
  message to a sender. Reading or injecting traffic on a network that carries
  SOME/IP is the protocol working as specified; protecting that network is the
  integrator's job.
- **Service discovery is unauthenticated.** An SD offer asserts that a service
  lives at an endpoint, and nothing in the protocol lets a client verify that
  assertion. A host able to send SD messages on the segment can offer a service
  it does not own and draw subscriptions to itself. This crate encodes and
  decodes SD entries; it cannot tell a legitimate offer from a spoofed one.
- **E2E protection is a safety mechanism, not a security one.** Profile 4
  (CRC-32) and Profile 5 (CRC-16) detect accidental corruption — bit flips,
  truncation, a stale or duplicated frame. A CRC is not a message
  authentication code: it is unkeyed, so anyone who can alter a payload can
  recompute the checksum over it. E2E protection does not make a channel
  tamper-resistant, and a report that it can be forged describes the profile
  rather than a bug here.

What is in scope: anything that makes the crate misbehave on attacker-supplied
bytes — a panic, an out-of-bounds read, an unbounded allocation, a decode that
accepts a frame it should reject, or a hang reachable from the wire. The
fixed-capacity `heapless` collections used by the SD codec make
bounds-handling on oversized or malformed entries a particular area of
interest.
