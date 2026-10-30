//! The link thread, as the clipboard thread sees it (docs/link.md, "In the daemon"): its events
//! arrive on a channel with a wake pipe the clipboard thread polls, and its state is kept here to
//! answer `jackstay peers` and `jackstay status` at once.

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use jackstay_net::{Event, Handle, PairFailure, PeerState, Status};
use jackstay_proto::control::{self, Dialer, LinkStatus, PairReason, PeerSummary, printable};

/// The link thread sends status at least every second; this long without any means it is wedged,
/// and the daemon restarts (docs/link.md, "Supervision").
pub const SILENT_FOR: Duration = Duration::from_secs(10);
/// A gap this long between the clipboard loop's checks means the whole process was paused
/// (suspend, a frozen VM, swapping): the link's silence then is not its own, and is forgiven.
const PAUSED: Duration = Duration::from_secs(3);
/// The link failing this many times in a row turns it off at the next start, so a fault that
/// recurs cannot keep restarting the clipboard manager until systemd gives up on it.
pub const FAILURES_ALLOWED: u32 = 3;
/// Running this long counts as a success: the failures are forgotten.
const HEALTHY_AFTER: Duration = Duration::from_secs(60);

pub struct Link {
    handle: Option<Handle>,
    /// Why the link is off, if it is.
    off: Option<String>,
    events: Receiver<Event>,
    /// Readable whenever an event is waiting.
    pub wake: UnixStream,
    status: Option<Status>,
    heard: Instant,
    /// When the clipboard loop last checked.
    checked: Instant,
    started: Instant,
    /// The file counting the link's failures in a row, while it still counts.
    failures: Option<PathBuf>,
}

impl std::fmt::Debug for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Link").field("off", &self.off).finish_non_exhaustive()
    }
}

/// How the link thread ended or went quiet, for the journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gone {
    /// The thread ended: it panicked.
    Ended,
    /// No status for [`SILENT_FOR`] while the daemon ran.
    Silent,
}

/// One line to the journal: states and fingerprints, never contents.
fn log(line: &str) {
    let _ = writeln!(std::io::stderr(), "{line}");
}

/// The link's failures in a row, as counted in `path`; none if there is no file.
#[must_use]
pub fn failures(path: &Path) -> u32 {
    std::fs::read_to_string(path).ok().and_then(|t| t.trim().parse().ok()).unwrap_or(0)
}

/// Counts one more failure in `path`.
pub fn failed(path: &Path) {
    let _ = std::fs::write(path, format!("{}\n", failures(path).saturating_add(1)));
}

fn text(t: &str) -> String {
    printable(t).chars().take(control::MAX_LINK_TEXT / 4).collect()
}

impl Link {
    /// Starts the link with `config`, or records why it is off: `config` is the reason when it is
    /// not to run, and the link may also fail to start (its identity or peers file unusable). The
    /// daemon runs on either way. Input sharing's reports go to `input`, on the link thread. `failures` counts its failures in a row (docs/link.md,
    /// "Supervision"); the file is removed once the link has run for a minute.
    ///
    /// # Errors
    /// The wake pipe could not be made.
    pub fn start(
        config: Result<jackstay_net::Config, String>,
        failures: Option<PathBuf>,
        input: jackstay_net::Sink,
    ) -> std::io::Result<Link> {
        let (tx, events) = mpsc::channel();
        let (wake, wake_tx) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        wake_tx.set_nonblocking(true)?;
        let (handle, off) = match config {
            Err(why) => (None, Some(why)),
            Ok(config) => {
                let mut wake_tx = wake_tx;
                let events = move |e| {
                    let _ = tx.send(e);
                    // Full means a wake-up is already pending.
                    let _ = wake_tx.write(&[1]);
                };
                match jackstay_net::start(config, events, move |i| input(i)) {
                    Ok(h) => (Some(h), None),
                    Err(e) => (None, Some(e.to_string())),
                }
            }
        };
        let now = Instant::now();
        Ok(Link {
            handle,
            off,
            events,
            wake,
            status: None,
            heard: now,
            checked: now,
            started: now,
            failures,
        })
    }

    #[must_use]
    pub fn handle(&self) -> Option<&Handle> {
        self.handle.as_ref()
    }

    /// Leaves the link thread without waiting for it, when it has stopped answering.
    pub fn abandon(mut self) {
        if let Some(h) = self.handle.take() {
            h.abandon();
        }
    }

    /// The events waiting, after keeping the latest status (and saying in the journal what
    /// changed in it).
    pub fn receive(&mut self) -> Vec<Event> {
        let mut drained = [0u8; 256];
        while matches!((&self.wake).read(&mut drained), Ok(n) if n > 0) {}
        let mut out = Vec::new();
        for e in self.events.try_iter() {
            self.heard = Instant::now();
            match e {
                Event::Status(s) => {
                    changes(self.status.as_ref(), &s);
                    self.status = Some(s);
                }
                other => out.push(other),
            }
        }
        out
    }

    /// Whether the link thread is still there and talking: `Err` if it ended, or went silent while
    /// the daemon ran.
    ///
    /// # Errors
    /// How it is gone.
    pub fn check(&mut self) -> Result<(), Gone> {
        let Some(handle) = &self.handle else { return Ok(()) };
        if self.checked.elapsed() >= PAUSED {
            // The whole process was paused: the link has had no time to speak.
            self.heard = Instant::now();
        }
        self.checked = Instant::now();
        if self.started.elapsed() >= HEALTHY_AFTER
            && let Some(path) = self.failures.take()
        {
            let _ = std::fs::remove_file(path);
        }
        if handle.finished() {
            Err(Gone::Ended)
        } else if self.heard.elapsed() >= SILENT_FOR {
            Err(Gone::Silent)
        } else {
            Ok(())
        }
    }

    /// The paired machines, by key and the name each gave itself.
    #[must_use]
    pub fn peers(&self) -> Vec<(jackstay_net::PublicKey, String)> {
        let peers = self.status.as_ref().map(|s| s.peers.as_slice()).unwrap_or_default();
        peers.iter().map(|p| (p.key, p.name.clone())).collect()
    }

    /// The paired machine `name_or_fingerprint` names: by fingerprint, or by a name only one has.
    ///
    /// # Errors
    /// [`control::Error::NoSuchPeer`] or [`control::Error::AmbiguousPeer`].
    pub fn find(
        &self,
        name_or_fingerprint: &str,
    ) -> Result<jackstay_net::PublicKey, control::Error> {
        let peers = self.status.as_ref().map(|s| s.peers.as_slice()).unwrap_or_default();
        if let Some(p) = peers.iter().find(|p| p.key.fingerprint() == name_or_fingerprint) {
            return Ok(p.key);
        }
        let mut named = peers.iter().filter(|p| p.name == name_or_fingerprint);
        match (named.next(), named.next()) {
            (Some(p), None) => Ok(p.key),
            (Some(_), Some(_)) => Err(control::Error::AmbiguousPeer),
            (None, _) => Err(control::Error::NoSuchPeer),
        }
    }

    /// The link's state for a client.
    #[must_use]
    pub fn link_status(&self) -> LinkStatus {
        let Some(status) = &self.status else {
            // Off with its reason; or running but not yet reported: no port yet.
            return LinkStatus { off: self.off.as_deref().map(text), ..LinkStatus::default() };
        };
        let peers = status
            .peers
            .iter()
            .take(control::MAX_PEERS)
            .map(|p| PeerSummary {
                name: text(&p.name),
                fingerprint: p.key.fingerprint(),
                address: text(&p.address),
                state: match p.state {
                    PeerState::Connected => control::PeerState::Connected,
                    PeerState::Suspect => control::PeerState::Suspect,
                    PeerState::Dialing => control::PeerState::Dialing,
                    PeerState::Refused(jackstay_net::Refusal::UnknownKey) => {
                        control::PeerState::RefusedKey
                    }
                    PeerState::Refused(jackstay_net::Refusal::Version) => {
                        control::PeerState::RefusedVersion
                    }
                },
                rtt_us: p.rtt.map(|d| u32::try_from(d.as_micros()).unwrap_or(u32::MAX)),
                connects: p.connects,
                dialed: p.dialed.map(|us| if us { Dialer::Us } else { Dialer::Peer }),
            })
            .collect();
        LinkStatus {
            off: self.off.as_deref().map(text),
            port: status.listening.as_ref().ok().copied(),
            bind_error: status.listening.as_ref().err().map(|e| text(e)),
            fingerprint: status.fingerprint.clone(),
            pairing: status.pairing,
            peers,
        }
    }
}

/// Writes to the journal what changed: the port opening or failing, and each peer's state.
fn changes(before: Option<&Status>, now: &Status) {
    if before.map(|b| &b.listening) != Some(&now.listening) {
        match &now.listening {
            Ok(port) => log(&format!("link: listening on UDP port {port}")),
            Err(why) => log(&format!("link: port not open ({}); retrying", text(why))),
        }
    }
    for p in &now.peers {
        let was = before.and_then(|b| b.peers.iter().find(|q| q.key == p.key)).map(|q| q.state);
        if was != Some(p.state) {
            log(&format!("link: {} {:?}", p.key.fingerprint(), p.state));
        }
    }
}

/// A pairing failure as the control protocol carries it.
#[must_use]
pub fn pair_reason(f: &PairFailure) -> (PairReason, String) {
    match f {
        PairFailure::Busy => (PairReason::Busy, String::new()),
        PairFailure::NoAddress => (PairReason::NoAddress, String::new()),
        PairFailure::Bind(e) => (PairReason::Bind, text(e)),
        PairFailure::Timeout => (PairReason::Timeout, String::new()),
        PairFailure::Declined => (PairReason::Declined, String::new()),
        PairFailure::PeerEnded => (PairReason::PeerEnded, String::new()),
        PairFailure::SecondAttempt => (PairReason::SecondAttempt, String::new()),
        PairFailure::BadReveal => (PairReason::BadReveal, String::new()),
        PairFailure::TooManyPeers => (PairReason::TooManyPeers, String::new()),
        PairFailure::Save(e) => (PairReason::Save, text(e)),
        PairFailure::Cancelled => (PairReason::Cancelled, String::new()),
    }
}
