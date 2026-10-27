//! The history as the daemon keeps it: which entry is on the clipboard now, how a capture's reports
//! become an entry, and the user's commands (docs/daemon.md, "History in the daemon", "Clearing").
//! No I/O: the session applies the engine's reports here, and saves the history when told it
//! changed.

use std::sync::Arc;

use jackstay_clip::{Content, EntryId, History, Source};
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
                self.open =
                    Some(Open { capture: *capture, recording: Recording::Nothing, last: None });
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
            Report::KeptAlive
            | Report::Claimed
            | Report::ClaimFailed
            | Report::ManagerConflict { .. }
            | Report::ManagerLost
            | Report::OwnerHung
            | Report::SaveAnswered { .. } => false,
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
                Some(if complete {
                    self.history.record(content, source, now_ms)
                } else {
                    self.history.record_partial(content, source, now_ms)
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

    /// Moves an entry to the front to be served again: returns its content for
    /// `Clipboard::serve`, whose outcome goes to [`Tracker::served`]. The history changed (the
    /// order, and what the limits evicted), so it needs saving.
    pub fn select(&mut self, id: EntryId) -> Option<Arc<Content>> {
        let (content, _evicted) = self.history.promote(id)?;
        self.settle();
        Some(content)
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
