use std::fmt;

/// An X atom. Zero is `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Atom(pub u32);

impl Atom {
    pub const NONE: Atom = Atom(0);
}

/// An X window id. Zero is `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Window(pub u32);

/// An X server timestamp: milliseconds that wrap every 49.7 days. Zero is `CurrentTime`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Timestamp(pub u32);

impl Timestamp {
    pub const CURRENT_TIME: Timestamp = Timestamp(0);

    /// Whether `self` is earlier than `other`, allowing for wrap-around: the nearer way round
    /// the 32-bit circle decides, as for X's own comparisons within half the range.
    #[must_use]
    pub fn is_earlier_than(self, other: Timestamp) -> bool {
        let ahead = other.0.wrapping_sub(self.0);
        ahead != 0 && ahead < 0x8000_0000
    }
}

/// Monotonic milliseconds, from the adapter's clock. Deadlines are in this unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Millis(pub u64);

impl Millis {
    #[must_use]
    pub fn after(self, ms: u64) -> Millis {
        Millis(self.0.saturating_add(ms))
    }
}

impl fmt::Display for Millis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ms", self.0)
    }
}

/// What an `XFixes` selection notification reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerChange {
    /// `SetSelectionOwner`: a client claimed (or released) the selection.
    Set,
    /// The owner's window was destroyed.
    WindowDestroyed,
    /// The owner's connection closed.
    ClientClosed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyState {
    NewValue,
    Deleted,
}

/// The events the engine consumes, decoded by the adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// `XFixes` `SelectionNotify`.
    OwnerChanged {
        selection: Atom,
        owner: Option<Window>,
        change: OwnerChange,
        /// The server time of the event.
        timestamp: Timestamp,
        /// The selection's last-change time (`lastTimeChanged`), which an owner's death leaves
        /// alone.
        selection_timestamp: Timestamp,
    },
    /// Core `SelectionNotify`: an owner's answer to our conversion.
    SelectionNotify {
        selection: Atom,
        target: Atom,
        property: Option<Atom>,
        time: Timestamp,
    },
    /// Core `SelectionRequest`: a client asking us, as owner, for a target.
    SelectionRequest {
        requestor: Window,
        selection: Atom,
        target: Atom,
        property: Option<Atom>,
        time: Timestamp,
    },
    /// Core `SelectionClear`: we lost a selection we owned.
    SelectionClear {
        selection: Atom,
        time: Timestamp,
    },
    PropertyNotify {
        window: Window,
        atom: Atom,
        state: PropertyState,
        time: Timestamp,
    },
    DestroyNotify {
        window: Window,
    },
}
