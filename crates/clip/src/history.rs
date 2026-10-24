use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;

use crate::Content;

/// Identifies one history entry. Never reused, across restarts too: a restored history continues
/// from the next id it was saved with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntryId(u64);

impl EntryId {
    #[must_use]
    pub fn new(raw: u64) -> Self {
        EntryId(raw)
    }

    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Where a copy came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// An application on this machine; its owner window's `WM_CLASS` class, when it had one.
    Local { owner_class: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    id: EntryId,
    content: Arc<Content>,
    source: Source,
    /// Milliseconds since the Unix epoch, UTC.
    copied_at_ms: u64,
    pinned: bool,
}

impl Entry {
    /// For restoring a saved history; see [`History::restore`].
    #[must_use]
    pub fn new(
        id: EntryId,
        content: Arc<Content>,
        source: Source,
        copied_at_ms: u64,
        pinned: bool,
    ) -> Self {
        Entry { id, content, source, copied_at_ms, pinned }
    }

    #[must_use]
    pub fn id(&self) -> EntryId {
        self.id
    }

    #[must_use]
    pub fn content(&self) -> &Arc<Content> {
        &self.content
    }

    #[must_use]
    pub fn source(&self) -> &Source {
        &self.source
    }

    #[must_use]
    pub fn copied_at_ms(&self) -> u64 {
        self.copied_at_ms
    }

    #[must_use]
    pub fn pinned(&self) -> bool {
        self.pinned
    }
}

/// How much unpinned history to keep. Pinned entries do not count, and are never evicted. The
/// newest entry does not count toward `max_bytes`, so one large copy never empties the history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_entries: NonZeroUsize,
    pub max_bytes: u64,
}

/// What a change to the history did. `evicted` entries are gone from the history; the caller
/// deletes whatever it stored for them.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct Recorded {
    pub id: EntryId,
    pub evicted: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreError {
    DuplicateId(EntryId),
    /// The saved next id is not above every saved entry's id, so an id could be reused.
    NextIdTooLow {
        next_id: u64,
        highest: u64,
    },
}

impl fmt::Display for RestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RestoreError::DuplicateId(id) => write!(f, "entry id {} appears twice", id.get()),
            RestoreError::NextIdTooLow { next_id, highest } => {
                write!(f, "next id {next_id} is not above the highest id {highest}")
            }
        }
    }
}

impl std::error::Error for RestoreError {}

/// The clipboard history, newest first. Each distinct content (by [`Content::same_kept`]) appears
/// at most once: copying something already in the history moves that entry to the front.
#[derive(Debug)]
pub struct History {
    entries: VecDeque<Entry>,
    limits: Limits,
    next_id: u64,
}

impl History {
    #[must_use]
    pub fn new(limits: Limits) -> Self {
        History { entries: VecDeque::new(), limits, next_id: 1 }
    }

    /// Rebuilds a saved history. Entries are newest first; limits are applied, and what they evict
    /// is returned.
    ///
    /// # Errors
    /// A [`RestoreError`] if ids repeat or `next_id` would let one be reused.
    pub fn restore(
        limits: Limits,
        entries: Vec<Entry>,
        next_id: u64,
    ) -> Result<(Self, Vec<Entry>), RestoreError> {
        let mut seen = HashSet::with_capacity(entries.len());
        if let Some(dup) = entries.iter().find(|e| !seen.insert(e.id)) {
            return Err(RestoreError::DuplicateId(dup.id));
        }
        if let Some(highest) = entries.iter().map(|e| e.id.0).max()
            && next_id <= highest
        {
            return Err(RestoreError::NextIdTooLow { next_id, highest });
        }
        let mut history = History { entries: entries.into(), limits, next_id: next_id.max(1) };
        let evicted = history.enforce_limits();
        Ok((history, evicted))
    }

    /// The id the next new entry will get; save it with the entries.
    #[must_use]
    pub fn next_id(&self) -> u64 {
        self.next_id
    }

    /// Entries, newest first.
    pub fn entries(&self) -> impl ExactSizeIterator<Item = &Entry> {
        self.entries.iter()
    }

    #[must_use]
    pub fn newest(&self) -> Option<&Entry> {
        self.entries.front()
    }

    #[must_use]
    pub fn get(&self, id: EntryId) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// The entry holding the same thing as `content`, if any.
    #[must_use]
    pub fn find(&self, content: &Content) -> Option<EntryId> {
        self.entries.iter().find(|e| e.content.same_kept(content)).map(|e| e.id)
    }

    /// Records a copy. If the history already holds the same thing, that entry moves to the front,
    /// keeping its id and pin, with this copy's source and time. Otherwise a new entry is added.
    ///
    /// # Panics
    /// After 2^64 - 1 new entries, rather than reuse an id.
    pub fn record(&mut self, content: Arc<Content>, source: Source, copied_at_ms: u64) -> Recorded {
        if let Some(at) = self.entries.iter().position(|e| e.content.same_kept(&content))
            && let Some(mut entry) = self.entries.remove(at)
        {
            entry.source = source;
            entry.copied_at_ms = copied_at_ms;
            let id = entry.id;
            self.entries.push_front(entry);
            return Recorded { id, evicted: self.enforce_limits() };
        }
        let id = EntryId(self.next_id);
        // 2^64 copies cannot happen; if it did, reusing an id would be worse than stopping.
        self.next_id = self.next_id.checked_add(1).expect("entry ids exhausted");
        self.entries.push_front(Entry { id, content, source, copied_at_ms, pinned: false });
        Recorded { id, evicted: self.enforce_limits() }
    }

    /// Moves an entry to the front, as when the user selects it to paste again.
    pub fn promote(&mut self, id: EntryId) -> Option<(Arc<Content>, Vec<Entry>)> {
        let at = self.entries.iter().position(|e| e.id == id)?;
        let entry = self.entries.remove(at)?;
        let content = Arc::clone(&entry.content);
        self.entries.push_front(entry);
        // The entry that was newest, and so outside the byte limit, no longer is.
        Some((content, self.enforce_limits()))
    }

    /// Pins or unpins an entry. Returns `None` if it does not exist, else what unpinning evicted.
    pub fn set_pinned(&mut self, id: EntryId, pinned: bool) -> Option<Vec<Entry>> {
        let entry = self.entries.iter_mut().find(|e| e.id == id)?;
        entry.pinned = pinned;
        Some(self.enforce_limits())
    }

    /// Removes one entry, pinned or not. Returns it if it existed.
    pub fn delete(&mut self, id: EntryId) -> Option<Entry> {
        let at = self.entries.iter().position(|e| e.id == id)?;
        self.entries.remove(at)
    }

    /// Removes every unpinned entry and returns them.
    pub fn clear_unpinned(&mut self) -> Vec<Entry> {
        let (kept, removed): (Vec<Entry>, Vec<Entry>) =
            self.entries.drain(..).partition(|e| e.pinned);
        self.entries = kept.into();
        removed
    }

    /// Changes the limits, returning what the new ones evict.
    pub fn set_limits(&mut self, limits: Limits) -> Vec<Entry> {
        self.limits = limits;
        self.enforce_limits()
    }

    /// Evicts the oldest unpinned entries, never the newest, until the unpinned entries number at
    /// most `max_entries` and those other than the newest hold at most `max_bytes`.
    fn enforce_limits(&mut self) -> Vec<Entry> {
        let mut count = self.entries.iter().filter(|e| !e.pinned).count();
        let mut bytes = self
            .entries
            .iter()
            .skip(1)
            .filter(|e| !e.pinned)
            .map(|e| e.content.kept_len())
            .fold(0, u64::saturating_add);
        let mut evicted = Vec::new();
        while count > self.limits.max_entries.get() || bytes > self.limits.max_bytes {
            let Some(at) = self.entries.iter().skip(1).rposition(|e| !e.pinned) else { break };
            let Some(entry) = self.entries.remove(at.saturating_add(1)) else { break };
            count = count.saturating_sub(1);
            bytes = bytes.saturating_sub(entry.content.kept_len());
            evicted.push(entry);
        }
        evicted
    }
}
