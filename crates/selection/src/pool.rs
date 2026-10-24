use crate::atoms::name;
use crate::{Atom, Millis, Port};

/// How long a property whose conversion was abandoned stays out of use. Owners keep transfers alive
/// after we stop listening (GTK 30 s, Chromium 10 s).
pub(crate) const QUARANTINE_MS: u64 = 60_000;
/// The most properties the pool will intern. Reached only by sustained abandonment.
pub(crate) const MAX_SLOTS: usize = 256;
/// Properties interned before any is reused, so that even a cleanly finished one is not handed out
/// again at once.
pub(crate) const MIN_SLOTS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Free,
    InUse,
    Quarantined { until: Millis },
}

/// Properties on our window for incoming transfers. Each conversion gets one no other transfer is
/// using or might still write to (docs/selection.md, "Properties for incoming transfers").
#[derive(Debug, Default)]
pub(crate) struct Pool {
    slots: Vec<(Atom, State)>,
    /// Where the round-robin search for a free slot starts.
    next: usize,
}

impl Pool {
    /// A property free for a new conversion: a new one until there are [`MIN_SLOTS`], then the next
    /// free one round-robin, then a new one again, up to [`MAX_SLOTS`].
    pub(crate) fn take(&mut self, port: &mut impl Port, now: Millis) -> Option<Atom> {
        if self.slots.len() >= MIN_SLOTS {
            let len = self.slots.len();
            for i in 0..len {
                let at = self.next.wrapping_add(i).checked_rem(len).unwrap_or(0);
                let Some(slot) = self.slots.get_mut(at) else { continue };
                let free = match slot.1 {
                    State::Free => true,
                    State::Quarantined { until } => until <= now,
                    State::InUse => false,
                };
                if free {
                    slot.1 = State::InUse;
                    self.next = at.wrapping_add(1);
                    return Some(slot.0);
                }
            }
        }
        if self.slots.len() >= MAX_SLOTS {
            return None;
        }
        let atom = port.intern(&name(&format!("JACKSTAY_P{}", self.slots.len()))?);
        self.slots.push((atom, State::InUse));
        Some(atom)
    }

    /// Whether `atom` is one of ours that no transfer is using: anything written to it now is a
    /// late write from an abandoned or finished transfer.
    pub(crate) fn is_idle(&self, atom: Atom) -> bool {
        self.slots.iter().any(|s| s.0 == atom && s.1 != State::InUse)
    }

    /// Properties handed out and not yet released or quarantined.
    pub(crate) fn in_use(&self) -> usize {
        self.slots.iter().filter(|s| s.1 == State::InUse).count()
    }

    /// The transfer finished cleanly; nothing more will be written to it.
    pub(crate) fn release(&mut self, atom: Atom) {
        self.set(atom, State::Free);
    }

    /// The transfer was abandoned; its owner may still write to it.
    pub(crate) fn quarantine(&mut self, atom: Atom, now: Millis) {
        self.set(atom, State::Quarantined { until: now.after(QUARANTINE_MS) });
    }

    fn set(&mut self, atom: Atom, state: State) {
        if let Some(slot) = self.slots.iter_mut().find(|s| s.0 == atom) {
            slot.1 = state;
        }
    }
}
