//! The primary's side of input sharing as a pure state machine (docs/input.md, "Spans" and
//! "Primary"): what X and the link report in, what to do to this machine's X server and what to
//! send out. Peers are named by `K`: the daemon's public keys, or anything a test likes.

use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

use jackstay_proto::link::{KeyEntry, MAX_MOTION, Message, Side};

use crate::keys::Set;
use crate::layout::{Edge, Layout};

/// At most one grab attempt this often: a push against a barrier while a menu holds a grab makes
/// hundreds of barrier hits a second.
pub const ATTEMPT_EVERY: Duration = Duration::from_millis(50);
/// While on a peer, the session's last frame older than this ends the span, whatever the link
/// thread says (docs/input.md, "Freshness, whatever the link thread says").
pub const STALE_AFTER: Duration = Duration::from_millis(300);
/// The keysyms of the two Shift keys, for the hotkeys that need Shift.
const SHIFT_L: u32 = 0xffe1;
const SHIFT_R: u32 = 0xffe2;
/// The middle of an edge, as a fraction: where `jackstay switch` enters.
const MIDDLE: u16 = 1 << 15;

/// A key that does something on the primary while on a peer: its keysym, with Shift or without.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub sym: u32,
    pub shift: bool,
}

/// What the input thread does, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Do<K> {
    /// Try to grab the pointer and keyboard, and answer with [`Primary::grabbed`].
    Grab,
    Ungrab,
    /// Move this machine's pointer to the pixel.
    Warp {
        x: i32,
        y: i32,
    },
    /// Put barriers on these edges (and only these): the sides with a neighbour.
    Barriers(Vec<Edge>),
    /// Send to `peer` on `session`.
    Send {
        peer: K,
        session: u64,
        message: Message,
    },
    /// The session with `peer` carries input now, or no longer: it pings faster.
    Active {
        peer: K,
        on: bool,
    },
}

/// Where this machine's pointer is: here, or on a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Where<K> {
    Here,
    /// Grabbing, waiting for X to say whether the grab took.
    Crossing(K),
    On {
        peer: K,
        locked: bool,
    },
}

/// What is known of one peer this machine may cross to.
#[derive(Debug, Clone, Default)]
struct Peer {
    /// The session kept with it.
    session: Option<u64>,
    suspect: bool,
    /// Its layout arrived on this session: its input side is running.
    layout: bool,
    /// Our keymap went out on this session.
    keymap_sent: bool,
}

/// A grab asked for, on the way to `peer`.
#[derive(Debug, Clone, Copy)]
struct Pending<K> {
    peer: K,
    side: Side,
    /// Where the pointer crossed, on this machine.
    at: (i32, i32),
}

/// The pointer on a peer.
#[derive(Debug, Clone)]
struct Span<K> {
    peer: K,
    session: u64,
    id: u32,
    /// The side of this machine it left by, and where.
    side: Side,
    at: (i32, i32),
    /// Where the pointer is parked while grabbed, and how far it has drifted from there, in
    /// 1/256 px.
    park: (i32, i32),
    drift: (i64, i64),
    locked: bool,
    /// The keys held here, as far as this span saw them go down and up.
    held: Set,
    /// Hotkeys pressed: their release is not sent either.
    swallowed: Set,
}

#[derive(Debug, Clone)]
pub struct Primary<K> {
    layout: Layout,
    neighbours: [Option<K>; 4],
    peers: HashMap<K, Peer>,
    keymap: (u8, Vec<KeyEntry>),
    return_key: Option<Hotkey>,
    lock_key: Option<Hotkey>,
    pending: Option<Pending<K>>,
    span: Option<Span<K>>,
    /// The last span id used: each crossing takes the next.
    last_span: u32,
    last_attempt: Option<Instant>,
}

fn index(side: Side) -> usize {
    match side {
        Side::Left => 0,
        Side::Right => 1,
        Side::Top => 2,
        Side::Bottom => 3,
    }
}

/// Whether a push of (`dx`, `dy`) points out of this machine through `side`.
fn outward(side: Side, dx: i32, dy: i32) -> bool {
    match side {
        Side::Left => dx < 0,
        Side::Right => dx > 0,
        Side::Top => dy < 0,
        Side::Bottom => dy > 0,
    }
}

/// One pixel inside an edge point on `side`: a pointer put back exactly on the edge would push
/// the barrier with its first motion outward.
fn inside(side: Side, (x, y): (i32, i32)) -> (i32, i32) {
    match side {
        Side::Left => (x.saturating_add(1), y),
        Side::Right => (x.saturating_sub(1), y),
        Side::Top => (x, y.saturating_add(1)),
        Side::Bottom => (x, y.saturating_sub(1)),
    }
}

fn along(side: Side, (x, y): (i32, i32)) -> i32 {
    match side {
        Side::Left | Side::Right => y,
        Side::Top | Side::Bottom => x,
    }
}

impl<K: Copy + Eq + Hash> Primary<K> {
    /// `neighbours`: the peer beyond each side (left, right, top, bottom), resolved to keys.
    #[must_use]
    pub fn new(
        layout: Layout,
        neighbours: [Option<K>; 4],
        keymap: (u8, Vec<KeyEntry>),
        return_key: Option<Hotkey>,
        lock_key: Option<Hotkey>,
    ) -> Primary<K> {
        Primary {
            layout,
            neighbours,
            peers: HashMap::new(),
            keymap,
            return_key,
            lock_key,
            pending: None,
            span: None,
            last_span: 0,
            last_attempt: None,
        }
    }

    #[must_use]
    pub fn position(&self) -> Where<K> {
        match (&self.pending, &self.span) {
            (_, Some(s)) => Where::On { peer: s.peer, locked: s.locked },
            (Some(p), None) => Where::Crossing(p.peer),
            (None, None) => Where::Here,
        }
    }

    /// The barriers for the current layout and neighbours.
    #[must_use]
    pub fn barriers(&self) -> Do<K> {
        let edges = Side::ALL
            .into_iter()
            .filter(|&s| self.neighbour(s).is_some())
            .map(|s| self.layout.edge(s))
            .collect();
        Do::Barriers(edges)
    }

    fn neighbour(&self, side: Side) -> Option<K> {
        self.neighbours.get(index(side)).copied().flatten()
    }

    /// The neighbours changed (a peer paired, unpaired or renamed). A span on a peer that is no
    /// longer a neighbour ends.
    pub fn neighbours(&mut self, neighbours: [Option<K>; 4]) -> Vec<Do<K>> {
        self.neighbours = neighbours;
        let mut out = Vec::new();
        if let Some(s) = &self.span
            && !self.neighbours.contains(&Some(s.peer))
        {
            out = self.go_home(true);
        }
        out.push(self.barriers());
        out
    }

    /// This machine's monitors changed: the barriers move. A primary sends no layout of its own:
    /// a peer's layout says it is a secondary, ready to be driven, and another primary's must not.
    pub fn layout(&mut self, layout: Layout) -> Vec<Do<K>> {
        self.layout = layout;
        vec![self.barriers()]
    }

    /// This machine's keymap changed: the peer the pointer is on gets it now, the others before
    /// their next crossing.
    pub fn keymap(&mut self, first: u8, keys: Vec<KeyEntry>) -> Vec<Do<K>> {
        self.keymap = (first, keys);
        for p in self.peers.values_mut() {
            p.keymap_sent = false;
        }
        let Some(s) = &self.span else { return Vec::new() };
        let (peer, session) = (s.peer, s.session);
        self.send_keymap(peer, session).into_iter().collect()
    }

    fn send_keymap(&mut self, peer: K, session: u64) -> Option<Do<K>> {
        let p = self.peers.get_mut(&peer)?;
        p.keymap_sent = true;
        let (first, keys) = self.keymap.clone();
        Some(Do::Send { peer, session, message: Message::Keymap { first, keys } })
    }

    /// A session with `peer` was kept: any span on the session before ends, and the peer is
    /// crossed to once its layout says it is a secondary.
    pub fn kept(&mut self, peer: K, session: u64) -> Vec<Do<K>> {
        let out = if self.span.as_ref().is_some_and(|s| s.peer == peer) {
            self.go_home(false)
        } else {
            Vec::new()
        };
        self.peers.insert(peer, Peer { session: Some(session), ..Peer::default() });
        out
    }

    /// The session with `peer` went silent, or was heard again. On silence the span ends, and
    /// `Exit` with the last span id goes out whether a span was open or not: on the ordered
    /// stream it follows whatever arrives late (docs/input.md, "Spans").
    pub fn suspect(&mut self, peer: K, session: u64, suspect: bool) -> Vec<Do<K>> {
        let Some(p) = self.peers.get_mut(&peer).filter(|p| p.session == Some(session)) else {
            return Vec::new();
        };
        let was = p.suspect;
        p.suspect = suspect;
        if !suspect || was {
            return Vec::new();
        }
        self.silent(peer, session)
    }

    fn silent(&mut self, peer: K, session: u64) -> Vec<Do<K>> {
        let mut out = Vec::new();
        if self.span.as_ref().is_some_and(|s| s.peer == peer) {
            out = self.go_home(false);
        }
        out.push(Do::Send { peer, session, message: Message::Exit { span: self.last_span } });
        out
    }

    /// The session with `peer` ended.
    pub fn ended(&mut self, peer: K, session: u64) -> Vec<Do<K>> {
        if self.peers.get(&peer).is_none_or(|p| p.session != Some(session)) {
            return Vec::new();
        }
        // A grab asked for meanwhile is let go when it takes: the peer has no session.
        self.peers.remove(&peer);
        if self.span.as_ref().is_some_and(|s| s.peer == peer) {
            self.go_home(false)
        } else {
            Vec::new()
        }
    }

    /// The session the pointer is on was last heard `age` ago: stale past [`STALE_AFTER`], and
    /// then treated as silent, even if the link thread never says so.
    pub fn freshness(&mut self, age: Duration) -> Vec<Do<K>> {
        let Some(s) = &self.span else { return Vec::new() };
        if age <= STALE_AFTER {
            return Vec::new();
        }
        // Not marked suspect: if the link thread is what stopped, nothing would clear that.
        let (peer, session) = (s.peer, s.session);
        self.silent(peer, session)
    }

    /// A message from `peer`.
    pub fn message(&mut self, peer: K, session: u64, message: &Message) -> Vec<Do<K>> {
        let Some(p) = self.peers.get_mut(&peer).filter(|p| p.session == Some(session)) else {
            return Vec::new();
        };
        match message {
            Message::Layout { monitors } => {
                p.layout = Layout::new(monitors).is_ok();
                Vec::new()
            }
            Message::Leave { span, side, fraction } => self.leave(peer, *span, *side, *fraction),
            // Only a secondary sends the rest; a primary ignores them.
            _ => Vec::new(),
        }
    }

    /// The pointer pushed against the barrier on `side` at (`x`, `y`) by (`dx`, `dy`).
    pub fn barrier(
        &mut self,
        side: Side,
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
        now: Instant,
    ) -> Vec<Do<K>> {
        if self.pending.is_some() || self.span.is_some() || !outward(side, dx, dy) {
            return Vec::new();
        }
        let Some(peer) = self.neighbour(side).filter(|k| self.ready(*k)) else { return Vec::new() };
        self.attempt(Pending { peer, side, at: (x, y) }, now)
    }

    fn ready(&self, peer: K) -> bool {
        self.peers.get(&peer).is_some_and(|p| p.session.is_some() && !p.suspect && p.layout)
    }

    fn attempt(&mut self, pending: Pending<K>, now: Instant) -> Vec<Do<K>> {
        if self.last_attempt.is_some_and(|t| now.saturating_duration_since(t) < ATTEMPT_EVERY) {
            return Vec::new();
        }
        self.last_attempt = Some(now);
        self.pending = Some(pending);
        vec![Do::Grab]
    }

    /// `jackstay switch PEER`: cross to `peer` at the middle of the edge that faces it.
    pub fn switch_to(&mut self, peer: K) -> Vec<Do<K>> {
        let mut out = if self.span.as_ref().is_some_and(|s| s.peer != peer) {
            self.go_home(true)
        } else {
            Vec::new()
        };
        if self.span.is_some() || self.pending.is_some() || !self.ready(peer) {
            return out;
        }
        let Some(side) = Side::ALL.into_iter().find(|&s| self.neighbour(s) == Some(peer)) else {
            return out;
        };
        let at = self.layout.edge(side).point(MIDDLE);
        // Not limited by the barrier's rate: the user asked.
        self.pending = Some(Pending { peer, side, at });
        out.push(Do::Grab);
        out
    }

    /// `jackstay switch` alone, or the return key: the pointer comes back where it crossed.
    pub fn switch_home(&mut self) -> Vec<Do<K>> {
        if self.span.is_some() { self.go_home(true) } else { Vec::new() }
    }

    /// X's answer to [`Do::Grab`]; with the modifier keys held and the locks on, read after the
    /// grab took.
    pub fn grabbed(&mut self, took: bool, modifiers: [u8; 32], locks: u8) -> Vec<Do<K>> {
        let Some(pending) = self.pending.take() else {
            // Nothing asked for it any longer (the session ended meanwhile).
            return if took { vec![Do::Ungrab] } else { Vec::new() };
        };
        if !took {
            return Vec::new();
        }
        let session = self.peers.get(&pending.peer).and_then(|p| p.session);
        let Some(session) = session.filter(|_| self.ready(pending.peer)) else {
            return vec![Do::Ungrab];
        };
        let mut out = Vec::new();
        if self.peers.get(&pending.peer).is_some_and(|p| !p.keymap_sent) {
            out.extend(self.send_keymap(pending.peer, session));
        }
        // Ends a drain the secondary may be in since a silence this side never noticed (it heard
        // the secondary all along): on the ordered stream, after anything late.
        out.push(Do::Send {
            peer: pending.peer,
            session,
            message: Message::Exit { span: self.last_span },
        });
        self.last_span = self.last_span.wrapping_add(1);
        let fraction = self.layout.edge(pending.side).fraction(along(pending.side, pending.at));
        out.push(Do::Send {
            peer: pending.peer,
            session,
            message: Message::Enter {
                span: self.last_span,
                side: pending.side.opposite(),
                fraction,
                modifiers,
                locks,
            },
        });
        let (x, y) = pending.at;
        let m = self.layout.nearest(x, y);
        let park = self.layout.monitors().get(m).map_or((x, y), |r| {
            (r.x.saturating_add(r.width / 2), r.y.saturating_add(r.height / 2))
        });
        out.push(Do::Warp { x: park.0, y: park.1 });
        out.push(Do::Active { peer: pending.peer, on: true });
        self.span = Some(Span {
            peer: pending.peer,
            session,
            id: self.last_span,
            side: pending.side,
            at: pending.at,
            park,
            drift: (0, 0),
            locked: false,
            held: Set::from_bitmap(&modifiers),
            swallowed: Set::default(),
        });
        out
    }

    /// Pointer motion while grabbed, in 1/256 px.
    pub fn motion(&mut self, dx: i32, dy: i32) -> Vec<Do<K>> {
        let layout = &self.layout;
        let Some(s) = &mut self.span else { return Vec::new() };
        let (dx, dy) = (dx.clamp(-MAX_MOTION, MAX_MOTION), dy.clamp(-MAX_MOTION, MAX_MOTION));
        let mut out = vec![Do::Send {
            peer: s.peer,
            session: s.session,
            message: Message::Motion { span: s.id, dx, dy },
        }];
        // The pointer really moves here, unseen: back to the park point before it reaches a
        // barrier, at a quarter of the way to its monitor's nearer edge.
        s.drift =
            (s.drift.0.saturating_add(i64::from(dx)), s.drift.1.saturating_add(i64::from(dy)));
        let m = layout.nearest(s.park.0, s.park.1);
        if let Some(r) = layout.monitors().get(m) {
            let limit = |size: i32| i64::from(size).saturating_mul(256) / 8;
            if s.drift.0.abs() > limit(r.width) || s.drift.1.abs() > limit(r.height) {
                s.drift = (0, 0);
                out.push(Do::Warp { x: s.park.0, y: s.park.1 });
            }
        }
        out
    }

    pub fn button(&mut self, button: u8, down: bool) -> Vec<Do<K>> {
        let Some(s) = &self.span else { return Vec::new() };
        vec![Do::Send {
            peer: s.peer,
            session: s.session,
            message: Message::Button { span: s.id, button, down },
        }]
    }

    /// A key while grabbed: the hotkeys act here and are never sent.
    pub fn key(&mut self, keycode: u8, down: bool) -> Vec<Do<K>> {
        let hotkey = if down { self.hotkey(keycode) } else { None };
        let Some(s) = &mut self.span else { return Vec::new() };
        if !down && s.swallowed.remove(keycode) {
            return Vec::new();
        }
        let message = match hotkey {
            None => {
                if down {
                    s.held.insert(keycode);
                } else {
                    s.held.remove(keycode);
                }
                Message::Key { span: s.id, keycode, down }
            }
            Some(Hot::Lock) => {
                s.swallowed.insert(keycode);
                s.locked = !s.locked;
                Message::Lock { span: s.id, on: s.locked }
            }
            Some(Hot::Return) => return self.go_home(true),
        };
        vec![Do::Send { peer: s.peer, session: s.session, message }]
    }

    /// Which hotkey `keycode` going down is, with Shift as held now.
    fn hotkey(&self, keycode: u8) -> Option<Hot> {
        let s = self.span.as_ref()?;
        let syms = |code: u8| -> Option<&KeyEntry> {
            self.keymap.1.get(usize::from(code.checked_sub(self.keymap.0)?))
        };
        let sym = syms(keycode)?.level1;
        let shift = s.held.iter().any(|k| {
            k != keycode && syms(k).is_some_and(|e| e.level1 == SHIFT_L || e.level1 == SHIFT_R)
        });
        let matches = |h: Option<Hotkey>| h.is_some_and(|h| h.sym == sym && h.shift == shift);
        if matches(self.lock_key) {
            Some(Hot::Lock)
        } else if matches(self.return_key) {
            Some(Hot::Return)
        } else {
            None
        }
    }

    /// The secondary says the pointer left it through its `side` at `fraction`: it comes back
    /// through ours facing it, unless locked there.
    fn leave(&mut self, peer: K, span: u32, side: Side, fraction: u16) -> Vec<Do<K>> {
        let Some(s) = self.span.as_ref().filter(|s| s.peer == peer && s.id == span && !s.locked)
        else {
            return Vec::new();
        };
        let here = side.opposite();
        // The pointer comes back by the side whose neighbour it is on; if that is not our side
        // facing the one it left by (the two configurations disagree), where it crossed.
        let to = if self.neighbour(here) == Some(peer) {
            inside(here, self.layout.edge(here).point(fraction))
        } else {
            inside(s.side, s.at)
        };
        self.span = None;
        vec![Do::Warp { x: to.0, y: to.1 }, Do::Ungrab, Do::Active { peer, on: false }]
    }

    /// The pointer comes back where it crossed; `tell` sends the peer `Exit`, which a session that
    /// is gone or silent does not get.
    fn go_home(&mut self, tell: bool) -> Vec<Do<K>> {
        let Some(s) = self.span.take() else { return Vec::new() };
        let mut out = Vec::new();
        if tell {
            out.push(Do::Send {
                peer: s.peer,
                session: s.session,
                message: Message::Exit { span: s.id },
            });
        }
        let (x, y) = inside(s.side, s.at);
        out.extend([Do::Warp { x, y }, Do::Ungrab, Do::Active { peer: s.peer, on: false }]);
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hot {
    Return,
    Lock,
}
