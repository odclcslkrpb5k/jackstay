//! The link between Jackstay machines (docs/link.md): each machine's identity, pairing, and one
//! authenticated QUIC session per paired peer, kept up for as long as the peer can be reached.
//!
//! [`start`] runs the link on a thread of its own, with a single-threaded tokio runtime. The daemon
//! drives it through the returned [`Handle`] and hears from it through the `events` callback, which
//! runs on the link thread and must not block. Input sharing has a callback of its own, `input`,
//! called from each session as it hears ([`Input`]), and sends with [`Handle::send`].
//!
//! Bytes from other machines are decoded only by `jackstay_proto::link`, after the handshake has
//! proved the sender holds a paired key. This crate does no arithmetic on lengths from the wire;
//! the panicking constructs are denied all the same.
#![deny(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::string_slice,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]

pub mod address;
mod engine;
pub mod identity;
mod input;
mod pairing;
pub mod peers;
mod session;
mod tls;

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::mpsc;

pub use address::Address;
pub use identity::{Identity, IdentityError, PublicKey};
pub use input::{Heard, Input, Sink, is_input};
pub use peers::{Peer, PeersError};
pub use tls::{ALPN, PAIR_ALPN};

/// The default UDP port; pairing uses the next one.
pub const DEFAULT_PORT: u16 = 24810;

/// Every interval the link keeps (docs/link.md, "Liveness"). Tests shorten some.
#[derive(Debug, Clone)]
pub struct Timings {
    /// QUIC keep-alive.
    pub keep_alive: Duration,
    /// QUIC idle timeout: a session silent this long is dead.
    pub idle_timeout: Duration,
    /// Ping interval while a session is idle.
    pub idle_ping: Duration,
    /// Ping interval while a session is active (input flows over it).
    pub active_ping: Duration,
    /// How long one dial attempt may take.
    pub dial_deadline: Duration,
    /// The first wait before redialing, doubled after each failure.
    pub backoff_first: Duration,
    /// The longest wait between dials.
    pub backoff_max: Duration,
    /// The longest wait between dials to a peer that refused us.
    pub refused_max: Duration,
    /// How long the machine with the larger key waits before dialing, so the preferred session
    /// usually comes up alone.
    pub defer_dial: Duration,
    /// How long pairing waits for the other machine and the user.
    pub pairing_window: Duration,
    /// How often a port that could not be bound is tried again.
    pub bind_retry: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Timings {
            keep_alive: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(10),
            idle_ping: Duration::from_secs(1),
            active_ping: Duration::from_millis(100),
            dial_deadline: Duration::from_millis(1500),
            backoff_first: Duration::from_millis(100),
            backoff_max: Duration::from_secs(2),
            refused_max: Duration::from_secs(60),
            defer_dial: Duration::from_millis(500),
            pairing_window: Duration::from_secs(120),
            bind_retry: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Where the identity and the peers file live (`$XDG_STATE_HOME/jackstay`).
    pub state_dir: PathBuf,
    /// The UDP port; pairing uses the next one.
    pub port: u16,
    /// This machine's name, sent to peers for display.
    pub name: String,
    pub timings: Timings,
}

/// What the link is doing, sent whenever it changes and at least every second.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// The port the link listens on, or why it cannot.
    pub listening: Result<u16, String>,
    pub fingerprint: String,
    pub peers: Vec<PeerStatus>,
    pub pairing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerStatus {
    pub key: PublicKey,
    pub name: String,
    pub address: String,
    pub state: PeerState,
    /// The last ping's round trip.
    pub rtt: Option<Duration>,
    /// Sessions kept with this peer since the link started: a number that keeps rising means a
    /// link that keeps dropping.
    pub connects: u32,
    /// Whether this machine dialed the kept session (QUIC's client, which may change its
    /// address); `None` without one.
    pub dialed: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerState {
    Connected,
    /// Connected, but nothing heard for three of the peer's ping intervals.
    Suspect,
    /// Not connected; dialing, or waiting for the peer to dial.
    Dialing,
    /// The peer refused the last session.
    Refused(Refusal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The peer does not know our key, or showed one we do not know.
    UnknownKey,
    /// The peer speaks another version of the link.
    Version,
}

/// Why pairing ended without recording a peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairFailure {
    /// Pairing was already running.
    Busy,
    /// The address resolves to nothing.
    NoAddress,
    /// The pairing port could not be bound.
    Bind(String),
    /// The other machine did not pair in time, or the user did not answer.
    Timeout,
    /// The user here said the codes differ.
    Declined,
    /// The other machine ended pairing first.
    PeerEnded,
    /// A second handshake, or one from another address, arrived: pairing is not safe to finish.
    SecondAttempt,
    /// The other machine's nonce did not match its commitment.
    BadReveal,
    /// Already paired with the most peers allowed.
    TooManyPeers,
    /// The peers file could not be saved.
    Save(String),
    Cancelled,
}

impl fmt::Display for PairFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PairFailure::Busy => write!(f, "pairing is already running"),
            PairFailure::NoAddress => write!(f, "the address resolves to nothing"),
            PairFailure::Bind(e) => write!(f, "the pairing port could not be opened: {e}"),
            PairFailure::Timeout => write!(f, "timed out"),
            PairFailure::Declined => write!(f, "declined here"),
            PairFailure::PeerEnded => write!(f, "the other machine ended pairing"),
            PairFailure::SecondAttempt => write!(
                f,
                "a second pairing attempt arrived; pairing stopped in case it is an attack"
            ),
            PairFailure::BadReveal => write!(f, "the other machine broke the pairing protocol"),
            PairFailure::TooManyPeers => {
                write!(f, "already paired with {} machines", peers::MAX_PEERS)
            }
            PairFailure::Save(e) => write!(f, "the peers file could not be saved: {e}"),
            PairFailure::Cancelled => write!(f, "cancelled"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Status(Status),
    /// Pairing reached the other machine: the user compares `code` on both and answers.
    PairCode {
        name: String,
        fingerprint: String,
        code: String,
    },
    Paired {
        name: String,
        fingerprint: String,
    },
    PairFailed(PairFailure),
    /// The answer to [`Handle::unpair`]: whether the peer was removed and the file saved.
    Unpaired {
        fingerprint: String,
        saved: Result<(), UnpairError>,
    },
}

/// Why a peer was not unpaired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnpairError {
    /// It was not paired (already unpaired, say).
    NotPaired,
    /// The peers file could not be saved; the peer is still paired.
    Save(String),
}

#[derive(Debug)]
pub(crate) enum Command {
    Pair(Address),
    Confirm(bool),
    CancelPairing,
    Unpair(PublicKey),
    SetActive(PublicKey, bool),
    Send { key: PublicKey, session: u64, message: jackstay_proto::link::Message },
    Shutdown,
}

/// Why the link could not start. The daemon runs on without it.
#[derive(Debug)]
pub enum StartError {
    Identity(IdentityError),
    Peers(PeersError),
    Thread(std::io::Error),
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartError::Identity(e) => write!(f, "{e}"),
            StartError::Peers(e) => write!(f, "{e}"),
            StartError::Thread(e) => write!(f, "link thread: {e}"),
        }
    }
}

impl std::error::Error for StartError {}

/// The running link. Dropping it closes every session, saying goodbye, and joins the thread.
#[derive(Debug)]
pub struct Handle {
    commands: mpsc::UnboundedSender<Command>,
    thread: Option<JoinHandle<()>>,
    public: PublicKey,
}

impl Handle {
    /// Starts pairing with the machine at `address` (docs/link.md, "Pairing").
    pub fn pair(&self, address: Address) {
        let _ = self.commands.send(Command::Pair(address));
    }

    /// The user's answer to [`Event::PairCode`].
    pub fn confirm(&self, yes: bool) {
        let _ = self.commands.send(Command::Confirm(yes));
    }

    pub fn cancel_pairing(&self) {
        let _ = self.commands.send(Command::CancelPairing);
    }

    pub fn unpair(&self, key: PublicKey) {
        let _ = self.commands.send(Command::Unpair(key));
    }

    /// Marks the session with `key` active (input flows over it) or idle, which sets how often it
    /// pings and so how fast silence is noticed.
    pub fn set_active(&self, key: PublicKey, active: bool) {
        let _ = self.commands.send(Command::SetActive(key, active));
    }

    /// Sends an input message ([`Sender::send`]).
    pub fn send(&self, key: PublicKey, session: u64, message: jackstay_proto::link::Message) {
        self.sender().send(key, session, message);
    }

    /// What input sharing's thread sends and marks sessions active with.
    #[must_use]
    pub fn sender(&self) -> Sender {
        Sender { commands: self.commands.clone() }
    }

    /// Our public key.
    #[must_use]
    pub fn public(&self) -> PublicKey {
        self.public
    }

    /// Whether the link thread has ended (it ends only by panicking, or by being dropped).
    #[must_use]
    pub fn finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl Handle {
    /// Leaves the link thread without waiting for it: for one that stopped answering, which
    /// joining would wait on for ever. The process is about to exit.
    pub fn abandon(mut self) {
        drop(self.thread.take());
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Input sharing's way to the link, from its own thread: it never waits.
#[derive(Debug, Clone)]
pub struct Sender {
    commands: mpsc::UnboundedSender<Command>,
}

impl Sender {
    /// Sends an input message ([`is_input`]; any other is not sent) to the peer with `key` on
    /// `session`, if that is still the session kept with it: a message meant for a session that
    /// was replaced is dropped, never sent on its successor.
    pub fn send(&self, key: PublicKey, session: u64, message: jackstay_proto::link::Message) {
        if is_input(&message) {
            let _ = self.commands.send(Command::Send { key, session, message });
        }
    }

    /// As [`Handle::set_active`].
    pub fn set_active(&self, key: PublicKey, active: bool) {
        let _ = self.commands.send(Command::SetActive(key, active));
    }
}

/// Starts the link on its own thread.
///
/// The identity is made on the first start. A panic on the link thread is not caught: it ends the
/// thread, which [`Handle::finished`] reports, and the daemon then exits (docs/link.md, "In the
/// daemon").
///
/// # Errors
/// A [`StartError`] if the identity or the peers file cannot be used, or the thread cannot start.
pub fn start(
    config: Config,
    events: impl FnMut(Event) + Send + 'static,
    input: impl Fn(Input) + Send + Sync + 'static,
) -> Result<Handle, StartError> {
    let identity = Identity::load_or_create(&config.state_dir.join("identity"))
        .map_err(StartError::Identity)?;
    let peers = peers::load(&config.state_dir.join("peers")).map_err(StartError::Peers)?;
    let public = identity.public();
    // A copied disk image pairs with itself: it would dial and accept its own sessions.
    if peers.iter().any(|p| p.key == public) {
        return Err(StartError::Peers(PeersError::OwnKey));
    }
    let (commands, receiver) = mpsc::unbounded_channel();
    // Made here, so a failure is the start's, and the link is off, rather than the thread's end.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(StartError::Thread)?;
    let thread = std::thread::Builder::new()
        .name("link".to_owned())
        .spawn(move || {
            engine::run(
                runtime,
                config,
                identity,
                peers,
                receiver,
                Box::new(events),
                Arc::new(input),
            );
        })
        .map_err(StartError::Thread)?;
    Ok(Handle { commands, thread: Some(thread), public })
}
