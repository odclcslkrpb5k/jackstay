//! What input sharing hears from the link (docs/input.md, "Spans", "Primary"): each kept
//! session's input messages and its liveness, in the order the session saw them, delivered from
//! the session's own task so that nothing waits behind the engine or the clipboard.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use jackstay_proto::link::Message;

use crate::identity::PublicKey;

/// One session's report to input sharing. A session reports only once the engine kept it, so
/// every report of a session lies between its `Kept` and its `Ended`; two sessions with one peer
/// can overlap while one replaces the other, and `session` tells them apart.
#[derive(Debug, Clone)]
pub enum Input {
    /// The engine kept this session with `key`: input to that peer goes over it from now on.
    Kept { key: PublicKey, session: u64, heard: Arc<Heard> },
    /// An input message from the peer ([`is_input`]).
    Message { key: PublicKey, session: u64, message: Message },
    /// Nothing heard for three of the peer's ping intervals (or our pings unanswered), or heard
    /// again.
    Suspect { key: PublicKey, session: u64, suspect: bool },
    /// The session ended: closed, replaced, refused or lost.
    Ended { key: PublicKey, session: u64 },
}

/// Where input reports go: called on the link thread, so it must not block.
pub type Sink = Arc<dyn Fn(Input) + Send + Sync>;

/// When a session last received anything, readable from any thread without a lock: the input
/// thread ends a span on a stale session even if the link thread is wedged and reports nothing
/// (docs/input.md, "Freshness, whatever the link thread says").
#[derive(Debug)]
pub struct Heard {
    epoch: Instant,
    /// Nanoseconds after `epoch`.
    at: AtomicU64,
}

impl Heard {
    pub(crate) fn new() -> Heard {
        Heard { epoch: Instant::now(), at: AtomicU64::new(0) }
    }

    pub(crate) fn stamp(&self) {
        let now = u64::try_from(self.epoch.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.at.store(now, Ordering::Relaxed);
    }

    /// How long ago the session last received a frame (or was kept, if it has received none
    /// since).
    #[must_use]
    pub fn age(&self) -> Duration {
        let at = Duration::from_nanos(self.at.load(Ordering::Relaxed));
        self.epoch.elapsed().saturating_sub(at)
    }
}

/// Whether `m` is one of input sharing's messages, the only ones [`crate::Handle::send`] sends
/// and the only ones besides the link's own a session accepts.
#[must_use]
pub fn is_input(m: &Message) -> bool {
    matches!(
        m,
        Message::Layout { .. }
            | Message::Keymap { .. }
            | Message::Enter { .. }
            | Message::Motion { .. }
            | Message::Button { .. }
            | Message::Key { .. }
            | Message::Leave { .. }
            | Message::Exit { .. }
            | Message::Lock { .. }
    )
}
