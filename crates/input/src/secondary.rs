//! The secondary's side of input sharing as a pure state machine (docs/input.md, "Spans" and
//! "Secondary"): the primary's messages in, what to inject out.

use jackstay_proto::link::{KeyEntry, LOCK_CAPS, LOCK_NUM, Side};

use crate::keys::{Keymap, Set, Translation};
use crate::layout::Layout;
use crate::pointer::{Pointer, Sides, Step};

/// What the input thread does to this machine's X server, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Absolute motion to this pixel.
    MoveTo {
        x: i32,
        y: i32,
    },
    Button {
        button: u8,
        down: bool,
    },
    /// This machine's keycode.
    Key {
        keycode: u8,
        down: bool,
    },
    /// Set Caps Lock and Num Lock.
    Locks {
        caps: bool,
        num: bool,
    },
    /// Tell the primary the pointer left (send [`jackstay_proto::link::Message::Leave`]).
    Leave {
        span: u32,
        side: Side,
        fraction: u16,
    },
}

/// The span the pointer is on this machine for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Span {
    id: u32,
    pointer: Pointer,
    /// For each of the primary's keycodes held, this machine's keycode it was pressed as: a
    /// release goes to the same key even if the keymaps changed in between.
    keys: Vec<Option<u8>>,
    /// Buttons injected down, and not yet up.
    buttons: Set,
    /// The pointer left; the rest of the span's messages are dropped.
    left: bool,
    /// Locked to this machine by the primary's lock key: it never leaves.
    locked: bool,
}

/// One secondary's state toward the primary it accepts input from.
#[derive(Debug, Clone)]
pub struct Secondary {
    layout: Layout,
    exits: Sides,
    here: Keymap,
    primary_keymap: Option<(u8, Vec<KeyEntry>)>,
    translation: Translation,
    span: Option<Span>,
    /// After silence on the session, what the primary sent meanwhile arrives late: everything is
    /// dropped until its `Exit`, which it sends when it notices the silence (docs/input.md,
    /// "Spans").
    draining: bool,
}

impl Secondary {
    /// `exits`: the sides whose neighbour is this primary.
    #[must_use]
    pub fn new(layout: Layout, exits: Sides, here: Keymap) -> Secondary {
        Secondary {
            layout,
            exits,
            here,
            primary_keymap: None,
            translation: Translation::none(),
            span: None,
            draining: false,
        }
    }

    /// Whether a span is open: the primary is driving this machine.
    #[must_use]
    pub fn active(&self) -> bool {
        self.span.as_ref().is_some_and(|s| !s.left)
    }

    /// The primary's keymap arrived.
    pub fn primary_keymap(&mut self, first: u8, keys: Vec<KeyEntry>) {
        self.translation = Translation::new(first, &keys, &self.here);
        self.primary_keymap = Some((first, keys));
    }

    /// This machine's keymap changed.
    pub fn keymap(&mut self, here: Keymap) {
        self.here = here;
        if let Some((first, keys)) = &self.primary_keymap {
            self.translation = Translation::new(*first, keys, &self.here);
        }
    }

    /// This machine's monitors changed: the pointer stays on one.
    pub fn layout(&mut self, layout: Layout) -> Vec<Action> {
        self.layout = layout;
        match &mut self.span {
            Some(s) if !s.left => {
                let before = s.pointer.pixel();
                s.pointer.relayout(&self.layout);
                let (x, y) = s.pointer.pixel();
                if (x, y) == before { Vec::new() } else { vec![Action::MoveTo { x, y }] }
            }
            _ => Vec::new(),
        }
    }

    /// The sides whose neighbour is this primary changed.
    pub fn exits(&mut self, exits: Sides) {
        self.exits = exits;
    }

    /// This machine's own mouse moved the pointer to (`x`, `y`): the primary's motion goes on
    /// from there.
    pub fn moved(&mut self, x: i32, y: i32) {
        let layout = &self.layout;
        if let Some(s) = self.span.as_mut().filter(|s| !s.left) {
            s.pointer = Pointer::at(layout, x, y);
        }
    }

    /// `Enter`: any span still open ends first; the pointer appears on `side` at `fraction`, the
    /// held modifiers are pressed and the locks set.
    pub fn enter(
        &mut self,
        span: u32,
        side: Side,
        fraction: u16,
        modifiers: &[u8; 32],
        locks: u8,
    ) -> Vec<Action> {
        if self.draining {
            return Vec::new();
        }
        let mut out = self.end();
        let pointer = Pointer::enter(&self.layout, side, fraction);
        let (x, y) = pointer.pixel();
        out.push(Action::MoveTo { x, y });
        out.push(Action::Locks { caps: locks & LOCK_CAPS != 0, num: locks & LOCK_NUM != 0 });
        let mut s = Span {
            id: span,
            pointer,
            keys: vec![None; 256],
            buttons: Set::default(),
            left: false,
            locked: false,
        };
        for code in Set::from_bitmap(modifiers).iter() {
            // Only keys the primary calls modifiers: any other would type.
            if self.translation.is_modifier(code) {
                out.extend(press(&mut s, code, self.translation.key(code)));
            }
        }
        self.span = Some(s);
        out
    }

    /// The open span, if `span` is its id and the pointer has not left.
    fn current(&mut self, span: u32) -> Option<&mut Span> {
        self.span.as_mut().filter(|s| s.id == span && !s.left)
    }

    pub fn motion(&mut self, span: u32, dx: i32, dy: i32) -> Vec<Action> {
        let (layout, exits) = (&self.layout, self.exits);
        let Some(s) = self.span.as_mut().filter(|s| s.id == span && !s.left) else {
            return Vec::new();
        };
        let before = s.pointer.pixel();
        // A drag to the edge stays a drag (plan §5.3).
        match s.pointer.move_by(layout, dx, dy, exits, s.buttons.is_empty() && !s.locked) {
            Step::At { x, y } if (x, y) != before => vec![Action::MoveTo { x, y }],
            Step::At { .. } => Vec::new(),
            Step::Left { side, fraction } => {
                s.left = true;
                let mut out = release(s);
                out.push(Action::Leave { span, side, fraction });
                out
            }
        }
    }

    pub fn button(&mut self, span: u32, button: u8, down: bool) -> Vec<Action> {
        let Some(s) = self.current(span) else { return Vec::new() };
        let changed = if down { s.buttons.insert(button) } else { s.buttons.remove(button) };
        if changed { vec![Action::Button { button, down }] } else { Vec::new() }
    }

    pub fn key(&mut self, span: u32, keycode: u8, down: bool) -> Vec<Action> {
        let local = self.translation.key(keycode);
        let Some(s) = self.current(span) else { return Vec::new() };
        if down { press(s, keycode, local) } else { lift(s, keycode) }
    }

    /// `Lock`: the primary's lock key locked the pointer to this machine, or unlocked it.
    pub fn lock(&mut self, span: u32, on: bool) {
        if let Some(s) = self.current(span) {
            s.locked = on;
        }
    }

    /// `Exit` for `span`: the primary took the pointer back. Any `Exit` also ends a drain: what
    /// follows it was sent after the primary noticed the silence.
    pub fn exit(&mut self, span: u32) -> Vec<Action> {
        self.draining = false;
        if self.span.as_ref().is_some_and(|s| s.id == span) { self.end() } else { Vec::new() }
    }

    /// The session went silent: the span ends, and what arrives late is dropped until the
    /// primary's `Exit`.
    pub fn suspect(&mut self) -> Vec<Action> {
        self.draining = true;
        self.end()
    }

    /// A new session: the span ends, and a new stream has no backlog to drop.
    pub fn reset(&mut self) -> Vec<Action> {
        self.draining = false;
        self.end()
    }

    /// The span ends whatever its id: the session is suspect, closed or replaced. Everything
    /// held is released.
    pub fn end(&mut self) -> Vec<Action> {
        self.span.take().map(|mut s| release(&mut s)).unwrap_or_default()
    }
}

/// Whether this machine's `local` key is held for any of the primary's keys.
fn held(s: &Span, local: u8) -> bool {
    s.keys.contains(&Some(local))
}

/// The primary's `code` went down, as this machine's `local`. Two of the primary's keys can be
/// one key here (athena's Caps Lock and Super are both `Super_L`): it goes down once, and up when
/// the last of them does.
fn press(s: &mut Span, code: u8, local: Option<u8>) -> Vec<Action> {
    let Some(local) = local else { return Vec::new() };
    let Some(slot) = s.keys.get_mut(usize::from(code)) else { return Vec::new() };
    if slot.is_some() {
        return Vec::new();
    }
    *slot = Some(local);
    // Held already, for another of the primary's keys, counting the one just recorded.
    if s.keys.iter().filter(|k| **k == Some(local)).count() > 1 {
        return Vec::new();
    }
    vec![Action::Key { keycode: local, down: true }]
}

/// The primary's `code` went up: its key here too, unless another still holds it.
fn lift(s: &mut Span, code: u8) -> Vec<Action> {
    let Some(local) = s.keys.get_mut(usize::from(code)).and_then(Option::take) else {
        return Vec::new();
    };
    if held(s, local) { Vec::new() } else { vec![Action::Key { keycode: local, down: false }] }
}

/// Releases everything `s` holds: buttons first, so that a drag ends where it is, then keys.
fn release(s: &mut Span) -> Vec<Action> {
    let mut out: Vec<Action> =
        s.buttons.iter().map(|button| Action::Button { button, down: false }).collect();
    let mut keys = Set::default();
    for k in s.keys.iter_mut().filter_map(Option::take) {
        keys.insert(k);
    }
    out.extend(keys.iter().map(|keycode| Action::Key { keycode, down: false }));
    s.buttons = Set::default();
    out
}
