//! A vsomeip peer in a container, driven over stdin/stdout. See
//! `tests/data/vsomeip-peer/README.md` for the command protocol.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use super::super::consts::PEER_IP;

/// Image tag; CI overrides it with the ghcr.io tag via `SIMPLE_SOMEIP_VSOMEIP_IMAGE`.
fn image() -> String {
    std::env::var("SIMPLE_SOMEIP_VSOMEIP_IMAGE")
        .unwrap_or_else(|_| "simple-someip-vsomeip-peer".to_owned())
}

/// One line of peer output: a kind (`READY`, `EVENT`, ...) followed by
/// `key=value` fields.
#[derive(Debug, Clone)]
pub struct PeerLine {
    pub kind: String,
    pub fields: HashMap<String, String>,
    pub raw: String,
}

impl PeerLine {
    fn parse(raw: &str) -> Self {
        let mut it = raw.split_whitespace();
        let kind = it.next().unwrap_or_default().to_owned();
        let fields = it
            .filter_map(|kv| {
                kv.split_once('=')
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
            })
            .collect();
        Self {
            kind,
            fields,
            raw: raw.to_owned(),
        }
    }

    /// The hex field `key`, with or without a `0x` prefix.
    pub fn hex(&self, key: &str) -> u32 {
        let v = self
            .fields
            .get(key)
            .unwrap_or_else(|| panic!("no `{key}` in `{}`", self.raw));
        u32::from_str_radix(v.trim_start_matches("0x"), 16)
            .unwrap_or_else(|e| panic!("`{key}` is not hex in `{}`: {e}", self.raw))
    }

    /// The `payload` field as bytes; `-` or a missing field is empty.
    pub fn payload(&self) -> Vec<u8> {
        match self.fields.get("payload").map(String::as_str) {
            None | Some("-") => Vec::new(),
            Some(h) => (0..h.len())
                .step_by(2)
                .map(|i| {
                    h.get(i..i + 2)
                        .and_then(|b| u8::from_str_radix(b, 16).ok())
                        .unwrap_or_else(|| panic!("payload is not hex in `{}`", self.raw))
                })
                .collect(),
        }
    }
}

/// A running vsomeip peer container. Dropping it removes the container.
pub struct VsomeipPeer {
    name: String,
    child: Child,
    stdin: ChildStdin,
    output: PeerOutput,
}

static NEXT: AtomicU32 = AtomicU32::new(0);

impl VsomeipPeer {
    /// Starts the peer and waits until vsomeip has registered.
    pub fn start() -> Self {
        let name = format!(
            "someip-interop-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let mut child = Command::new("docker")
            .args(["run", "-i", "--rm", "--network", "host", "--name", &name])
            .args(["-e", &format!("VSOMEIP_UNICAST={PEER_IP}")])
            .arg(image())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| {
                panic!("could not run docker ({e}); is Docker installed and running?")
            });
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut peer = Self {
            name,
            child,
            stdin,
            output: PeerOutput::new(lines),
        };
        peer.output.expect_with_hint(
            "READY",
            Duration::from_secs(20),
            &format!(
                "Is the image `{}` available? If not, build the image first: docker build --network=host -t simple-someip-vsomeip-peer tests/data/vsomeip-peer/",
                image()
            ),
        );
        peer
    }

    /// Sends one command and waits for the peer to accept it.
    pub fn send(&mut self, cmd: &str) {
        writeln!(self.stdin, "{cmd}").expect("peer stdin closed");
        let verb = cmd.split_whitespace().next().expect("empty peer command");
        let line = self.output.next_matching(
            |l| l.kind == "OK" || l.kind == "ERR",
            Duration::from_secs(5),
        );
        match line {
            Some(l) if l.raw.strip_prefix("OK ") == Some(verb) => {}
            Some(l) => panic!("peer rejected `{cmd}`: {}", l.raw),
            None => panic!(
                "peer did not acknowledge `{cmd}`; {}",
                self.output.transcript()
            ),
        }
    }

    /// See [`PeerOutput::clear`].
    pub fn clear(&mut self) {
        self.output.clear();
    }

    /// See [`PeerOutput::expect`].
    pub fn expect(&mut self, kind: &str, timeout: Duration) -> PeerLine {
        self.output.expect(kind, timeout)
    }

    /// See [`PeerOutput::expect_where`].
    pub fn expect_where(
        &mut self,
        kind: &str,
        timeout: Duration,
        pred: impl Fn(&PeerLine) -> bool,
    ) -> PeerLine {
        self.output.expect_where(kind, timeout, pred)
    }

    /// See [`PeerOutput::next_where`].
    pub fn next_where(
        &mut self,
        kind: &str,
        timeout: Duration,
        pred: impl Fn(&PeerLine) -> bool,
    ) -> Option<PeerLine> {
        self.output.next_where(kind, timeout, pred)
    }

    /// See [`PeerOutput::expect_none`].
    pub fn expect_none(&mut self, kind: &str, within: Duration) {
        self.output.expect_none(kind, within);
    }
}

/// The lines a peer prints, read from a channel.
///
/// Lines are kept until a call consumes them, so a line that arrives while
/// waiting for something else (for example `AVAILABLE` before `OK offer`) is
/// still there for a later [`expect`](Self::expect). To look only at lines
/// that follow an action, call [`clear`](Self::clear) before the action.
pub struct PeerOutput {
    lines: Receiver<String>,
    pending: VecDeque<PeerLine>,
    seen: Vec<String>,
    exited: bool,
}

impl PeerOutput {
    pub fn new(lines: Receiver<String>) -> Self {
        Self {
            lines,
            pending: VecDeque::new(),
            seen: Vec::new(),
            exited: false,
        }
    }

    /// Discards every line received so far, consumed or not. Later calls
    /// match only lines that arrive after this one. The discarded lines stay
    /// in the failure transcript.
    pub fn clear(&mut self) {
        loop {
            match self.lines.try_recv() {
                Ok(raw) => self.seen.push(raw),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.exited = true;
                    break;
                }
            }
        }
        self.pending.clear();
    }

    /// Waits for the oldest unconsumed line whose kind is `kind`.
    ///
    /// The match may be a line received before this call; call
    /// [`clear`](Self::clear) first to look only at later lines.
    pub fn expect(&mut self, kind: &str, timeout: Duration) -> PeerLine {
        self.expect_with_hint(kind, timeout, "")
    }

    /// Waits for the oldest unconsumed line of `kind` that also satisfies
    /// `pred`.
    ///
    /// The match may be a line received before this call; call
    /// [`clear`](Self::clear) first to look only at later lines.
    pub fn expect_where(
        &mut self,
        kind: &str,
        timeout: Duration,
        pred: impl Fn(&PeerLine) -> bool,
    ) -> PeerLine {
        self.next_matching(|l| l.kind == kind && pred(l), timeout)
            .unwrap_or_else(|| {
                panic!(
                    "peer never reported a matching {kind} within {timeout:?}; {}",
                    self.transcript()
                )
            })
    }

    /// Like [`expect_where`](Self::expect_where), but returns `None` when no
    /// matching line arrives within `timeout`, for a caller that reports the
    /// absence itself.
    pub fn next_where(
        &mut self,
        kind: &str,
        timeout: Duration,
        pred: impl Fn(&PeerLine) -> bool,
    ) -> Option<PeerLine> {
        self.next_matching(|l| l.kind == kind && pred(l), timeout)
    }

    /// Asserts no unconsumed line of `kind` exists or arrives within
    /// `within`, and that the peer is still running afterwards.
    ///
    /// A line received before this call counts; call [`clear`](Self::clear)
    /// first to look only at later lines.
    pub fn expect_none(&mut self, kind: &str, within: Duration) {
        match self.next_matching(|l| l.kind == kind, within) {
            Some(l) => panic!(
                "peer unexpectedly reported: {}; {}",
                l.raw,
                self.transcript()
            ),
            None if self.exited => panic!(
                "peer exited during expect_none({kind}); {}",
                self.transcript()
            ),
            None => {}
        }
    }

    fn expect_with_hint(&mut self, kind: &str, timeout: Duration, hint: &str) -> PeerLine {
        self.next_matching(|l| l.kind == kind, timeout)
            .unwrap_or_else(|| {
                panic!(
                    "peer never reported {kind} within {timeout:?}. {hint}\n{}",
                    self.transcript()
                )
            })
    }

    /// Removes and returns the oldest unconsumed line matching `pred`, waiting
    /// up to `timeout` for one to arrive. Non-matching lines stay pending.
    fn next_matching(
        &mut self,
        pred: impl Fn(&PeerLine) -> bool,
        timeout: Duration,
    ) -> Option<PeerLine> {
        if let Some(i) = self.pending.iter().position(&pred) {
            return self.pending.remove(i);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(raw) => {
                    let line = PeerLine::parse(&raw);
                    self.seen.push(raw);
                    if pred(&line) {
                        return Some(line);
                    }
                    self.pending.push_back(line);
                }
                Err(RecvTimeoutError::Timeout) => return None,
                Err(RecvTimeoutError::Disconnected) => {
                    self.exited = true;
                    return None;
                }
            }
        }
    }

    fn transcript(&self) -> String {
        let exited = if self.exited {
            "the peer has exited; "
        } else {
            ""
        };
        if self.seen.is_empty() {
            format!("{exited}the peer has printed nothing")
        } else {
            format!("{exited}peer output so far:\n  {}", self.seen.join("\n  "))
        }
    }
}

impl Drop for VsomeipPeer {
    fn drop(&mut self) {
        let _ = writeln!(self.stdin, "quit");
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = self.child.wait();
    }
}
