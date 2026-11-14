//! The history as the daemon keeps it: which entry is on the clipboard now, how a capture's reports
//! become an entry, and the user's commands (docs/daemon.md, "History in the daemon", "Clearing").
//! No I/O: the session applies the engine's reports here, and saves the history when told it
//! changed.

use std::sync::Arc;

use jackstay_clip::{Content, Entry, EntryId, History, Origin, Source};
use jackstay_selection::Report;

/// What the capture being recorded has become.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recording {
    /// No content reported yet.
    Nothing,
    /// Reported, as this entry.
    Entry(EntryId),
    /// Its entry was deleted or evicted: its later reports are ignored.
    Gone,
}

#[derive(Debug)]
struct Open {
    capture: u64,
    /// Taken at the owner change, so the copy is ordered by when it started (docs/sharing.md,
    /// "Order").
    tick: u64,
    recording: Recording,
    /// The content last reported, which completes the entry if the capture is cut short.
    last: Option<Arc<Content>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinError {
    NoSuchEntry,
    /// Pinning it would pin more than the configured maximum.
    TooManyPinned,
}

#[derive(Debug)]
pub struct Tracker {
    history: History,
    max_pinned: usize,
    open: Option<Open>,
    current: Option<EntryId>,
    cleared: bool,
}

impl Tracker {
    /// Starts from a history, typically restored; `cleared` as saved.
    #[must_use]
    pub fn new(history: History, max_pinned: usize, cleared: bool) -> Self {
        Tracker { history, max_pinned, open: None, current: None, cleared }
    }

    #[must_use]
    pub fn history(&self) -> &History {
        &self.history
    }

    /// The entry on the clipboard now, if Jackstay knows it.
    #[must_use]
    pub fn current(&self) -> Option<EntryId> {
        self.current
    }

    /// Whether the clipboard was cleared since anything was last put on it, so a restart must not
    /// put anything back.
    #[must_use]
    pub fn cleared(&self) -> bool {
        self.cleared
    }

    /// Applies one report from the engine. Returns whether the history or the cleared flag changed,
    /// so it needs saving.
    pub fn on_report(&mut self, report: &Report, now_ms: u64) -> bool {
        match report {
            Report::CaptureStarted { capture } => {
                let changed = self.complete_open();
                let tick = self.history.next_tick(now_ms);
                let recording = Recording::Nothing;
                self.open = Some(Open { capture: *capture, tick, recording, last: None });
                self.current = None;
                changed
            }
            Report::Captured { capture, content, source, complete } => {
                self.captured(*capture, content, source.as_ref(), *complete, now_ms)
            }
            Report::CaptureEnded { capture } | Report::CaptureFailed { capture } => {
                if self.open.as_ref().is_some_and(|o| o.capture == *capture) {
                    self.complete_open()
                } else {
                    false
                }
            }
            Report::Lost => {
                self.current = None;
                false
            }
            // Given up by its owner: nothing is on the clipboard, and a restart puts nothing back.
            Report::Emptied => self.emptied(),
            Report::KeptAlive
            | Report::Claimed
            | Report::ClaimFailed
            | Report::ManagerConflict { .. }
            | Report::ManagerLost
            | Report::OwnerHung
            | Report::SaveAnswered { .. }
            | Report::Wanted { .. } => false,
        }
    }

    fn captured(
        &mut self,
        capture: u64,
        content: &Arc<Content>,
        source: Option<&String>,
        complete: bool,
        now_ms: u64,
    ) -> bool {
        let Some(open) = self.open.as_mut().filter(|o| o.capture == capture) else { return false };
        let recorded = match open.recording {
            Recording::Gone => return false,
            Recording::Nothing => {
                let source = Source::Local { owner_class: source.cloned() };
                let content = Arc::clone(content);
                let tick = open.tick;
                Some(if complete {
                    self.history.record_at(content, source, now_ms, tick)
                } else {
                    self.history.record_partial_at(content, source, now_ms, tick)
                })
            }
            Recording::Entry(id) => self.history.update(id, Arc::clone(content), complete),
        };
        let id = recorded.map(|r| r.id);
        open.recording = id.map_or(Recording::Gone, Recording::Entry);
        open.last = Some(Arc::clone(content));
        if complete {
            self.open = None;
        }
        self.current = id;
        self.cleared = false;
        self.settle();
        true
    }

    /// Completes the open capture's entry with what it last reported, so it is deduplicated like a
    /// capture that ran to the end.
    fn complete_open(&mut self) -> bool {
        let Some(open) = self.open.take() else { return false };
        let (Recording::Entry(id), Some(last)) = (open.recording, open.last) else { return false };
        let merged = self.history.update(id, last, true).map(|r| r.id);
        if self.current == Some(id) {
            self.current = merged;
        }
        self.settle();
        true
    }

    /// Forgets the current entry and the open capture's entry if the history no longer holds them.
    fn settle(&mut self) {
        let history = &self.history;
        if self.current.is_some_and(|id| history.get(id).is_none()) {
            self.current = None;
        }
        if let Some(open) = &mut self.open
            && let Recording::Entry(id) = open.recording
            && history.get(id).is_none()
        {
            open.recording = Recording::Gone;
        }
    }

    /// Moves an entry to the front to be served again, as the change at `tick`: returns its
    /// content for `Clipboard::serve`, whose outcome goes to [`Tracker::served`]. The history
    /// changed (the order, and what the limits evicted), so it needs saving.
    pub fn select(&mut self, id: EntryId, tick: u64) -> Option<Arc<Content>> {
        let (content, _evicted) = self.history.promote(id, tick)?;
        self.settle();
        Some(content)
    }

    /// The next tick on the hybrid clock, for a change made now (docs/sharing.md, "Order").
    pub fn next_tick(&mut self, now_ms: u64) -> u64 {
        self.history.next_tick(now_ms)
    }

    /// A tick another machine used: this machine's next ones are later.
    pub fn observe(&mut self, tick: u64) {
        self.history.observe(tick);
    }

    /// Whether a copy is being captured: a change from a peer waits for it (docs/sharing.md,
    /// "Order").
    #[must_use]
    pub fn capturing(&self) -> bool {
        self.open.is_some()
    }

    /// The clipboard emptied, by its owner or by a peer: nothing is current, and a restart puts
    /// nothing back; history keeps every entry. Returns whether that changed what is saved.
    pub fn emptied(&mut self) -> bool {
        self.current = None;
        !std::mem::replace(&mut self.cleared, true)
    }

    /// Records a copy a peer announced ([`History::record_received`]): its entry's id, or `None`
    /// if it was deleted since.
    pub fn received(
        &mut self,
        content: Arc<Content>,
        source: Source,
        copied_at_ms: u64,
        tick: u64,
    ) -> Option<EntryId> {
        let recorded = self.history.record_received(content, source, copied_at_ms, tick)?;
        self.settle();
        self.history.get(recorded.id).map(Entry::id)
    }

    /// Replaces an entry's content with the same copy, more of it held (a target fetched).
    /// Returns `None` if the entry is gone.
    pub fn fill(&mut self, id: EntryId, content: Arc<Content>) -> Option<()> {
        let _evicted = self.history.update(id, content, false)?;
        self.settle();
        self.history.get(id).map(|_| ())
    }

    /// Remembers a deletion, so an older announcement of it is refused ([`History::was_forgotten`]),
    /// with where the entry came from, which says who hears of it.
    pub fn remember_forgotten(&mut self, digest: [u8; 32], tick: u64, origin: Origin) {
        self.history.remember_forgotten(digest, tick, origin);
    }

    /// Records whether serving `id` (after [`Tracker::select`] or at a restore) took the
    /// clipboard. Returns whether the cleared flag changed.
    pub fn served(&mut self, id: EntryId, claimed: bool) -> bool {
        if !claimed || self.history.get(id).is_none() {
            return false;
        }
        self.current = Some(id);
        std::mem::replace(&mut self.cleared, false)
    }

    /// After the clipboard was cleared: the current entry goes too, unless it is pinned.
    pub fn clipboard_cleared(&mut self) {
        if let Some(id) = self.current.take()
            && self.history.get(id).is_some_and(|e| !e.pinned())
        {
            self.history.delete(id);
        }
        self.cleared = true;
        self.settle();
    }

    /// Deletes an entry, pinned or not. Returns `None` if there is none, else whether it was on the
    /// clipboard, which must then be cleared.
    pub fn delete(&mut self, id: EntryId) -> Option<bool> {
        self.history.delete(id)?;
        let was_current = self.current == Some(id);
        self.settle();
        Some(was_current)
    }

    /// Deletes every unpinned entry; the clipboard must then be cleared. Returns how many went.
    pub fn clear_history(&mut self) -> usize {
        let removed = self.history.clear_unpinned().len();
        self.settle();
        removed
    }

    /// # Errors
    /// A [`PinError`] if there is no such entry or pinning it would pass the maximum.
    pub fn pin(&mut self, id: EntryId, pinned: bool) -> Result<(), PinError> {
        let entry = self.history.get(id).ok_or(PinError::NoSuchEntry)?;
        let count = self.history.entries().filter(|e| e.pinned()).count();
        if pinned && !entry.pinned() && count >= self.max_pinned {
            return Err(PinError::TooManyPinned);
        }
        self.history.set_pinned(id, pinned);
        self.settle();
        Ok(())
    }

    /// Adds an imported copy as the oldest entry. Returns whether it was added (and kept).
    pub fn import(&mut self, content: Arc<Content>, source: Source, copied_at_ms: u64) -> bool {
        let Some(recorded) = self.history.append_oldest(content, source, copied_at_ms) else {
            return false;
        };
        self.settle();
        self.history.get(recorded.id).is_some()
    }
}
