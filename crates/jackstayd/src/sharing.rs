//! Clipboard sharing in the daemon (docs/sharing.md): what is announced to peers, what their
//! announcements do here, and which target is fetched next. No I/O: the session hands in the
//! engine's reports, the user's commands and the link's events, and carries out what comes back
//! ([`Do`]).

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jackstay_clip::{
    Content, EntryId, Format, Kept, Remote, Source, Target, TargetData, TargetName,
};
use jackstay_net::{Clip, FetchFailed, PublicKey};
use jackstay_proto::link::Message;
use jackstay_proto::sharing::{self, Announce, MAX_FORGETS, Name};
use jackstay_selection::Report;

use crate::tracker::Tracker;

/// Targets up to this size travel inside the announcement.
const INLINE: usize = sharing::MAX_INLINE_LEN;
/// A capture still running this long after its first content is announced with what it has.
const EARLY_AFTER: Duration = Duration::from_secs(1);
/// More captures than this announced within a second is a loop with another tool, or as bad.
const LOOP_RATE: usize = 10;
/// How long announcing captures stops after [`LOOP_RATE`] is passed.
const LOOP_PAUSE: Duration = Duration::from_secs(10);
/// A received tick further ahead of the wall clock than this is refused.
const AHEAD: u64 = 24 * 60 * 60 * 1000;

/// What the session carries out for sharing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Do {
    /// Sends `message` to the peer with `key` on `session` (`Handle::share`).
    Share { key: PublicKey, session: u64, message: Message },
    /// Fetches a target (`Handle::fetch`).
    Fetch { key: PublicKey, session: u64, fetch: u64, digest: [u8; 32], name: Name, size: u64 },
    /// Stops a fetch (`Handle::cancel_fetch`).
    Cancel { fetch: u64 },
    /// Answers a peer's fetch (`Handle::answer`).
    Answer { request: u64, bytes: Option<Arc<[u8]>> },
    /// Makes this entry the clipboard (`Clipboard::serve`, then `Tracker::served`).
    Serve(EntryId),
    /// Serves this entry's new content in place of the old, without claiming again
    /// (`Clipboard::update`).
    Update(EntryId),
    /// Empties the clipboard, keeping history (`Clipboard::clear`, then `Tracker::emptied`).
    Empty,
    /// The history changed: save it.
    Save,
}

/// A change, as the clipboard state orders it: `(tick, origin)`, the same everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Change {
    tick: u64,
    origin: PublicKey,
    /// The entry it put on the clipboard, or none: emptied.
    digest: Option<[u8; 32]>,
}

impl Change {
    fn newer_than(&self, other: Option<&Change>) -> bool {
        other.is_none_or(|o| (self.tick, self.origin) > (o.tick, o.origin))
    }
}

/// The capture here being announced.
#[derive(Debug)]
struct Capture {
    id: u64,
    /// When its first content was reported.
    first: Option<Instant>,
    /// The digest it was announced with, early or complete.
    announced: Option<[u8; 32]>,
}

/// One target to fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Want {
    key: PublicKey,
    digest: [u8; 32],
    name: TargetName,
    size: u64,
    sha256: [u8; 32],
    /// A paste waits for it.
    urgent: bool,
}

#[derive(Debug)]
struct Running {
    fetch: u64,
    want: Want,
}

/// Clipboard sharing's state on this machine.
#[derive(Debug)]
pub struct Sharing {
    us: PublicKey,
    /// The largest target fetched; larger ones stay to come, and a paste of one is refused.
    per_target: u64,
    /// Peers whose session is kept now, by the session's number.
    sessions: BTreeMap<PublicKey, u64>,
    /// The newest change applied here.
    state: Option<Change>,
    capture: Option<Capture>,
    /// A change from a peer, newer than the state, waiting for a capture here to end.
    held: Option<Change>,
    wants: Vec<Want>,
    running: Option<Running>,
    next_fetch: u64,
    /// Targets the peer did not hold or sent wrong, by SHA-256: not asked for again until
    /// jackstayd restarts.
    failed: HashSet<[u8; 32]>,
    /// When the last captures were announced, within the last second.
    recent: VecDeque<Instant>,
    paused_until: Option<Instant>,
}

/// One line to the journal: digests by prefix and sizes, never contents (DP-3).
fn log(line: &str) {
    let _ = writeln!(std::io::stderr(), "{line}");
}

fn short(digest: &[u8; 32]) -> String {
    digest.iter().take(4).fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// The 32 bytes of an Ed25519 key that history keeps, from a peer's key as it travels.
#[must_use]
pub fn raw_key(key: &PublicKey) -> [u8; 32] {
    let bytes = key.as_bytes();
    let tail = bytes.get(bytes.len().saturating_sub(32)..).unwrap_or_default();
    tail.try_into().unwrap_or([0; 32])
}

/// What an announcement says, as [`Content`]; `None` if it is not what it claims to be: a name or
/// format this machine cannot hold, or a digest that does not match the targets (inline bytes
/// included). A target already held here, by its hash, is taken from `held`.
fn content_of(a: &Announce, held: &dyn Fn(&Remote) -> Option<Kept>) -> Option<Content> {
    let mut targets = Vec::with_capacity(a.targets.len());
    for t in &a.targets {
        let name = TargetName::from_latin1(t.name.latin1()).ok()?;
        let type_name = TargetName::from_latin1(t.type_name.latin1()).ok()?;
        let format = Format::from_bits(t.format).ok()?;
        // Inline bytes need no check of their own: the digest is worked out from them.
        let data = if let Some(bytes) = &t.inline {
            TargetData::Kept(Kept::new(type_name, format, bytes.clone()).ok()?)
        } else {
            let remote = Remote { type_name, format, size: t.size, sha256: t.sha256 };
            held(&remote).map_or(TargetData::Remote(remote), TargetData::Kept)
        };
        targets.push(Target { name, data });
    }
    let content = Content::new(targets).ok()?;
    (content.digest() == a.digest).then_some(content)
}

/// The announcement of `content`, made at `tick`; `None` if it cannot be sent (a name the link
/// cannot carry, or too many long names for one message).
fn announcement(
    content: &Content,
    tick: u64,
    replaces: Option<[u8; 32]>,
    copied_at_ms: u64,
) -> Option<Announce> {
    let mut targets = Vec::new();
    let mut inline_total = 0usize;
    for t in content.targets() {
        let (type_name, format, size, sha256, bytes) = match &t.data {
            TargetData::Kept(k) => {
                let size = u64::try_from(k.bytes().len()).ok()?;
                (k.type_name(), k.format(), size, k.sha256(), Some(k.bytes()))
            }
            TargetData::Remote(r) => (&r.type_name, r.format, r.size, r.sha256, None),
            TargetData::NotKept(_) => continue,
        };
        // Text first, as captured: what fits goes inline.
        let inline = bytes.filter(|b| {
            b.len() <= INLINE && inline_total.saturating_add(b.len()) <= sharing::MAX_INLINE_TOTAL
        });
        if let Some(b) = inline {
            inline_total = inline_total.saturating_add(b.len());
        }
        targets.push(sharing::Target {
            name: Name::new(&t.name.to_latin1()).ok()?,
            type_name: Name::new(&type_name.to_latin1()).ok()?,
            format: format.bits(),
            size,
            sha256,
            inline: inline.map(<[u8]>::to_vec),
        });
    }
    let mut a = Announce { tick, digest: content.digest(), replaces, copied_at_ms, targets };
    let fits = |a: &Announce| {
        let mut out = Vec::new();
        a.check().is_ok()
            && jackstay_proto::link::encode(&Message::Announce(Box::new(a.clone())), &mut out)
                .is_ok()
    };
    if !fits(&a) {
        // Too many long names: the bytes are fetched instead.
        for t in &mut a.targets {
            t.inline = None;
        }
    }
    fits(&a).then_some(a)
}

impl Sharing {
    /// Sharing for the machine with key `us`, starting from the tracker's clipboard: the entry it
    /// restored, as this machine's change at its tick.
    #[must_use]
    pub fn new(us: PublicKey, tracker: &Tracker, per_target: u64) -> Self {
        let history = tracker.history();
        let state = tracker.current().and_then(|id| history.get(id)).map(|e| Change {
            tick: e.tick(),
            origin: us,
            digest: Some(e.content().digest()),
        });
        Sharing {
            us,
            per_target,
            sessions: BTreeMap::new(),
            state,
            capture: None,
            held: None,
            wants: Vec::new(),
            running: None,
            next_fetch: 0,
            failed: HashSet::new(),
            recent: VecDeque::new(),
            paused_until: None,
        }
    }

    /// Whether announcing captures is paused, a loop with another tool suspected.
    #[must_use]
    pub fn paused(&self, now: Instant) -> bool {
        self.paused_until.is_some_and(|until| now < until)
    }

    /// Targets of entries from peers still to come: queued or being fetched.
    #[must_use]
    pub fn to_come(&self) -> usize {
        self.wants.len().saturating_add(usize::from(self.running.is_some()))
    }

    /// When [`Sharing::tick`] is next needed.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        let early = self
            .capture
            .as_ref()
            .filter(|c| c.announced.is_none())
            .and_then(|c| c.first)
            .map(|first| first + EARLY_AFTER);
        [early, self.paused_until].into_iter().flatten().min()
    }

    fn share(&self, out: &mut Vec<Do>, message: &Message) {
        for (&key, &session) in &self.sessions {
            out.push(Do::Share { key, session, message: message.clone() });
        }
    }

    /// The engine's reports, after the tracker has applied them.
    pub fn reports(
        &mut self,
        reports: &[Report],
        tracker: &mut Tracker,
        now: Instant,
        wall_ms: u64,
    ) -> Vec<Do> {
        let mut out = Vec::new();
        for r in reports {
            match r {
                Report::CaptureStarted { capture } => {
                    let capture = *capture;
                    self.capture = Some(Capture { id: capture, first: None, announced: None });
                }
                Report::Captured { capture, complete, .. } => {
                    let Some(c) = self.capture.as_mut().filter(|c| c.id == *capture) else {
                        continue;
                    };
                    c.first.get_or_insert(now);
                    if *complete {
                        self.announce_capture(tracker, now, &mut out);
                        self.capture = None;
                    }
                }
                Report::CaptureEnded { capture } | Report::CaptureFailed { capture } => {
                    if self.capture.as_ref().is_some_and(|c| c.id == *capture) {
                        if matches!(r, Report::CaptureEnded { .. }) {
                            self.announce_capture(tracker, now, &mut out);
                        }
                        self.capture = None;
                    }
                }
                Report::Emptied => {
                    self.capture = None;
                    let tick = tracker.next_tick(wall_ms);
                    self.state = Some(Change { tick, origin: self.us, digest: None });
                    log("clipboard given up by its owner; announced empty");
                    self.share(&mut out, &Message::Emptied { tick });
                }
                Report::Wanted { target } => self.wanted(tracker, target, &mut out),
                Report::KeptAlive
                | Report::Claimed
                | Report::ClaimFailed
                | Report::Lost
                | Report::ManagerConflict { .. }
                | Report::ManagerLost
                | Report::OwnerHung
                | Report::SaveAnswered { .. } => {}
            }
        }
        if !tracker.capturing() {
            self.apply_held(tracker, &mut out);
        }
        self.pump(tracker, &mut out);
        out
    }

    /// Time passing: a capture still running is announced with what it has.
    pub fn tick(&mut self, tracker: &mut Tracker, now: Instant) -> Vec<Do> {
        let mut out = Vec::new();
        if self.paused_until.is_some_and(|until| now >= until) {
            self.paused_until = None;
            log("sharing: announcing copies again");
        }
        let due = self.capture.as_ref().is_some_and(|c| {
            c.announced.is_none() && c.first.is_some_and(|f| now >= f + EARLY_AFTER)
        });
        if due {
            self.announce_capture(tracker, now, &mut out);
        }
        out
    }

    /// Announces the capture's entry as it is now, unless it is no change.
    fn announce_capture(&mut self, tracker: &Tracker, now: Instant, out: &mut Vec<Do>) {
        let Some(capture) = &mut self.capture else { return };
        let history = tracker.history();
        let Some(entry) = tracker.current().and_then(|id| history.get(id)) else { return };
        let digest = entry.content().digest();
        if capture.announced == Some(digest) {
            return;
        }
        let early = capture.announced.replace(digest);
        // Our own clipboard copied back by another tool (CopyQ, a VM's agent): no change.
        if early.is_none() && self.state.is_some_and(|s| s.digest == Some(digest)) {
            return;
        }
        self.state = Some(Change { tick: entry.tick(), origin: self.us, digest: Some(digest) });
        if early.is_none() {
            while self
                .recent
                .front()
                .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(1))
            {
                self.recent.pop_front();
            }
            self.recent.push_back(now);
            if self.recent.len() > LOOP_RATE && !self.paused(now) {
                self.paused_until = Some(now + LOOP_PAUSE);
                log(&format!(
                    "sharing: more than {LOOP_RATE} copies a second; not announcing copies for {} s",
                    LOOP_PAUSE.as_secs()
                ));
            }
        }
        if self.paused(now) {
            return;
        }
        match announcement(entry.content(), entry.tick(), early, entry.copied_at_ms()) {
            Some(a) => {
                log(&format!(
                    "announced {}: {} targets{}",
                    short(&digest),
                    a.targets.len(),
                    if early.is_some() { ", completing an early announcement" } else { "" }
                ));
                self.share(out, &Message::Announce(Box::new(a)));
            }
            None => log(&format!("{}: too many long target names to announce", short(&digest))),
        }
    }

    /// The user re-selected entry `id` at `tick`: a change, announced whatever its origin.
    pub fn selected(&mut self, tracker: &Tracker, id: EntryId) -> Vec<Do> {
        let mut out = Vec::new();
        let Some(entry) = tracker.history().get(id) else { return out };
        let digest = entry.content().digest();
        self.state = Some(Change { tick: entry.tick(), origin: self.us, digest: Some(digest) });
        if let Some(a) = announcement(entry.content(), entry.tick(), None, entry.copied_at_ms()) {
            log(&format!("announced {}: selected again", short(&digest)));
            self.share(&mut out, &Message::Announce(Box::new(a)));
        }
        out
    }

    /// The user deleted entries here at `tick` (`jackstay delete`, or `jackstay clear` deleting
    /// the clipboard's): deleted everywhere, and remembered.
    pub fn deleted(&mut self, tracker: &mut Tracker, digests: &[[u8; 32]], tick: u64) -> Vec<Do> {
        let mut out = Vec::new();
        for d in digests {
            tracker.remember_forgotten(*d, tick);
            if let Some(s) = self.state.as_mut().filter(|s| s.digest == Some(*d)) {
                s.digest = None;
            }
        }
        self.settle(tracker, &mut out);
        if !digests.is_empty() {
            log(&format!("forgot {} entries everywhere", digests.len()));
            self.share(&mut out, &Message::Forget { tick, digests: digests.to_vec() });
        }
        out
    }

    /// The clipboard was emptied here at `tick` (`jackstay clear`, `jackstay clear-history`):
    /// emptied everywhere.
    pub fn emptied(&mut self, tick: u64) -> Vec<Do> {
        let mut out = Vec::new();
        self.state = Some(Change { tick, origin: self.us, digest: None });
        self.share(&mut out, &Message::Emptied { tick });
        out
    }

    /// An event from the link. `name` is what the peer with a key calls itself.
    pub fn clip(
        &mut self,
        event: Clip,
        tracker: &mut Tracker,
        name: &dyn Fn(&PublicKey) -> String,
        wall_ms: u64,
    ) -> Vec<Do> {
        let mut out = Vec::new();
        match event {
            Clip::Up { key, session } => {
                self.sessions.insert(key, session);
                self.replay(tracker, key, session, &mut out);
                // Everything still to come, from the peer now here (two machines: the one
                // that announced it).
                let entries: Vec<(Arc<Content>, [u8; 32])> = tracker
                    .history()
                    .entries()
                    .filter(|e| {
                        e.content()
                            .targets()
                            .iter()
                            .any(|t| matches!(t.data, TargetData::Remote(_)))
                    })
                    .map(|e| (Arc::clone(e.content()), e.content().digest()))
                    .collect();
                for (content, digest) in entries {
                    self.want(key, digest, &content);
                }
            }
            Clip::Down { key, session } => {
                if self.sessions.get(&key) == Some(&session) {
                    self.sessions.remove(&key);
                }
            }
            Clip::Received { key, message, .. } => {
                self.received(tracker, key, message, name, wall_ms, &mut out);
            }
            Clip::Wanted { request, digest, name, .. } => {
                let bytes = tracker.history().find_digest(&digest).and_then(|id| {
                    let content = tracker.history().get(id)?.content();
                    let name = TargetName::from_latin1(name.latin1()).ok()?;
                    content.kept(name.as_str()).map(Kept::shared)
                });
                out.push(Do::Answer { request, bytes });
            }
            Clip::Fetched { fetch, result } => self.fetched(tracker, fetch, result, &mut out),
        }
        self.pump(tracker, &mut out);
        out
    }

    /// A new session: what this machine forgot, then its clipboard, so a peer that was away
    /// catches up (docs/sharing.md, "Forget").
    fn replay(&self, tracker: &Tracker, key: PublicKey, session: u64, out: &mut Vec<Do>) {
        let history = tracker.history();
        // One message per deletion's tick, so each is remembered at its own.
        let mut by_tick: BTreeMap<u64, Vec<[u8; 32]>> = BTreeMap::new();
        for (digest, tick) in history.forgotten() {
            by_tick.entry(*tick).or_default().push(*digest);
        }
        for (tick, digests) in by_tick {
            for chunk in digests.chunks(MAX_FORGETS) {
                let message = Message::Forget { tick, digests: chunk.to_vec() };
                out.push(Do::Share { key, session, message });
            }
        }
        let Some(state) = self.state else { return };
        let message = match state.digest {
            None => Message::Emptied { tick: state.tick },
            Some(d) => {
                let Some(entry) = history.find_digest(&d).and_then(|id| history.get(id)) else {
                    return;
                };
                let Some(a) = announcement(entry.content(), state.tick, None, entry.copied_at_ms())
                else {
                    return;
                };
                Message::Announce(Box::new(a))
            }
        };
        out.push(Do::Share { key, session, message });
    }

    fn received(
        &mut self,
        tracker: &mut Tracker,
        key: PublicKey,
        message: Message,
        name: &dyn Fn(&PublicKey) -> String,
        wall_ms: u64,
        out: &mut Vec<Do>,
    ) {
        let tick = match &message {
            Message::Announce(a) => a.tick,
            Message::Emptied { tick } | Message::Forget { tick, .. } => *tick,
            _ => return,
        };
        if tick > wall_ms.saturating_add(AHEAD) {
            log(&format!("{}: a change a day ahead of this clock; refused", key.fingerprint()));
            return;
        }
        match message {
            Message::Announce(a) => self.announced(tracker, key, &a, name, out),
            Message::Emptied { tick } => {
                tracker.observe(tick);
                self.change(tracker, Change { tick, origin: key, digest: None }, out);
            }
            Message::Forget { tick, digests } => {
                tracker.observe(tick);
                let mut forgot = 0usize;
                for d in &digests {
                    tracker.remember_forgotten(*d, tick);
                    let Some(id) = tracker.history().find_digest(d) else { continue };
                    forgot = forgot.saturating_add(1);
                    if tracker.delete(id) == Some(true) {
                        out.push(Do::Empty);
                    }
                    if let Some(s) = self.state.as_mut().filter(|s| s.digest == Some(*d)) {
                        s.digest = None;
                    }
                    if self.held.is_some_and(|h| h.digest == Some(*d)) {
                        self.held = None;
                    }
                }
                if forgot > 0 {
                    log(&format!("{}: forgot {forgot} entries", key.fingerprint()));
                    out.push(Do::Save);
                }
                self.settle(tracker, out);
            }
            _ => {}
        }
    }

    fn announced(
        &mut self,
        tracker: &mut Tracker,
        key: PublicKey,
        a: &Announce,
        name: &dyn Fn(&PublicKey) -> String,
        out: &mut Vec<Do>,
    ) {
        let history = tracker.history();
        let held = |r: &Remote| {
            history.entries().flat_map(|e| e.content().targets()).find_map(|t| match &t.data {
                TargetData::Kept(k)
                    if u64::try_from(k.bytes().len()).ok() == Some(r.size)
                        && k.type_name() == &r.type_name
                        && k.format() == r.format
                        && k.sha256() == r.sha256 =>
                {
                    Some(k.clone())
                }
                _ => None,
            })
        };
        let Some(content) = content_of(a, &held) else {
            let who = key.fingerprint();
            log(&format!("{who}: an announcement that does not match its digest; refused"));
            return;
        };
        let content = Arc::new(content);
        // The early announcement this one completes, if this machine still holds it from there,
        // unpinned: dropped, and the complete entry served in its place.
        let early = a.replaces.and_then(|d| history.find_digest(&d)).filter(|id| {
            history.get(*id).is_some_and(|e| {
                !e.pinned()
                    && matches!(e.source(), Source::Peer { key: k, .. } if *k == raw_key(&key))
            })
        });
        let mut in_place = false;
        if let Some(id) = early {
            in_place = tracker.current() == Some(id);
            tracker.delete(id);
        }
        let source = Source::Peer { key: raw_key(&key), name: name(&key) };
        let (who, digest) = (key.fingerprint(), short(&a.digest));
        let Some(id) = tracker.received(content, source, a.copied_at_ms, a.tick) else {
            log(&format!("{who}: announced {digest} after it was deleted here; refused"));
            return;
        };
        log(&format!("received {digest} from {who}: {} targets", a.targets.len()));
        out.push(Do::Save);
        let change = Change { tick: a.tick, origin: key, digest: Some(a.digest) };
        if in_place && self.state.is_some_and(|s| s.origin == key && s.tick == a.tick) {
            // The same change, complete: served in place, no new claim.
            if tracker.select(id, a.tick).is_some() {
                self.state = Some(change);
                out.push(Do::Update(id));
            }
        } else {
            self.change(tracker, change, out);
        }
        if let Some(entry) = tracker.history().get(id) {
            let content = Arc::clone(entry.content());
            self.want(key, a.digest, &content);
        }
    }

    /// A change from a peer: applied if newer than the clipboard state, after any capture here.
    fn change(&mut self, tracker: &mut Tracker, change: Change, out: &mut Vec<Do>) {
        if !change.newer_than(self.state.as_ref()) {
            return;
        }
        // What is on the clipboard already: a peer's replay of the change it took from here.
        if self.state.is_some_and(|s| s.digest == change.digest) && !tracker.capturing() {
            self.state = Some(change);
            return;
        }
        if tracker.capturing() {
            if change.newer_than(self.held.as_ref()) {
                self.held = Some(change);
            }
            return;
        }
        self.apply(tracker, change, out);
    }

    fn apply(&mut self, tracker: &mut Tracker, change: Change, out: &mut Vec<Do>) {
        match change.digest {
            None => out.push(Do::Empty),
            Some(d) => {
                let Some(id) = tracker.history().find_digest(&d) else { return };
                if tracker.select(id, change.tick).is_none() {
                    return;
                }
                out.push(Do::Serve(id));
                out.push(Do::Save);
            }
        }
        self.state = Some(change);
    }

    /// The change held while a capture ran, if it is still the newer.
    fn apply_held(&mut self, tracker: &mut Tracker, out: &mut Vec<Do>) {
        if let Some(held) = self.held.take()
            && held.newer_than(self.state.as_ref())
        {
            self.apply(tracker, held, out);
        }
    }

    /// Queues the targets of `content` (whose digest is `digest`) still to come from `key`.
    fn want(&mut self, key: PublicKey, digest: [u8; 32], content: &Content) {
        for t in content.targets() {
            let TargetData::Remote(r) = &t.data else { continue };
            if r.size > self.per_target || self.failed.contains(&r.sha256) {
                continue;
            }
            let queued = self.wants.iter().chain(self.running.as_ref().map(|r| &r.want));
            if queued.into_iter().any(|w| w.digest == digest && w.name == t.name) {
                continue;
            }
            self.wants.push(Want {
                key,
                digest,
                name: t.name.clone(),
                size: r.size,
                sha256: r.sha256,
                urgent: false,
            });
        }
    }

    /// A paste here waits for `target` of the clipboard's entry: its fetch goes first.
    fn wanted(&mut self, tracker: &Tracker, target: &TargetName, out: &mut Vec<Do>) {
        let Some(digest) = self.state.and_then(|s| s.digest) else { return };
        if self.running.as_ref().is_some_and(|r| r.want.digest == digest && r.want.name == *target)
        {
            return;
        }
        let history = tracker.history();
        if !self.wants.iter().any(|w| w.digest == digest && w.name == *target)
            && let Some(entry) = history.find_digest(&digest).and_then(|id| history.get(id))
        {
            // Failed before, or its session ended: asked for once more, from whoever announced
            // it.
            let from = match entry.source() {
                Source::Peer { key, .. } => {
                    self.sessions.keys().find(|k| raw_key(k) == *key).copied()
                }
                Source::Local { .. } => None,
            };
            if let Some(key) = from.or_else(|| self.sessions.keys().next().copied()) {
                for t in entry.content().targets().iter().filter(|t| t.name == *target) {
                    if let TargetData::Remote(r) = &t.data {
                        self.failed.remove(&r.sha256);
                    }
                }
                let content = Arc::clone(entry.content());
                self.want(key, digest, &content);
            }
        }
        for w in &mut self.wants {
            if w.digest == digest && w.name == *target {
                w.urgent = true;
            }
        }
        if let Some(running) = self.running.take_if(|r| !r.want.urgent) {
            out.push(Do::Cancel { fetch: running.fetch });
            self.wants.push(running.want);
        }
    }

    fn fetched(
        &mut self,
        tracker: &mut Tracker,
        fetch: u64,
        result: Result<Arc<[u8]>, FetchFailed>,
        out: &mut Vec<Do>,
    ) {
        let Some(running) = self.running.take_if(|r| r.fetch == fetch) else { return };
        let w = running.want;
        let bytes = match result {
            Ok(bytes) => bytes,
            Err(FetchFailed::Lost) => {
                // Asked for again on the next session.
                log(&format!("fetching {} {}: the session ended", short(&w.digest), w.size));
                return;
            }
            Err(why) => {
                log(&format!("fetching {}: {why:?}", short(&w.digest)));
                self.failed.insert(w.sha256);
                return;
            }
        };
        let history = tracker.history();
        let Some(id) = history.find_digest(&w.digest) else { return };
        let Some(entry) = history.get(id) else { return };
        let mut targets = entry.content().targets().to_vec();
        let Some(target) = targets.iter_mut().find(|t| t.name == w.name) else { return };
        let TargetData::Remote(r) = &target.data else { return };
        let kept = match Kept::new(r.type_name.clone(), r.format, bytes) {
            Ok(k) if k.sha256() == w.sha256 => k,
            _ => {
                log(&format!("fetching {}: the bytes do not match their hash", short(&w.digest)));
                self.failed.insert(w.sha256);
                return;
            }
        };
        target.data = TargetData::Kept(kept);
        let Ok(content) = Content::new(targets) else { return };
        if tracker.fill(id, Arc::new(content)).is_none() {
            return;
        }
        log(&format!("fetched {}: {} bytes", short(&w.digest), w.size));
        out.push(Do::Save);
        if tracker.current() == Some(id) {
            out.push(Do::Update(id));
        }
        self.settle(tracker, out);
    }

    /// Drops fetches for entries no longer held: deleted, or evicted.
    fn settle(&mut self, tracker: &Tracker, out: &mut Vec<Do>) {
        if self.running.is_none() && self.wants.is_empty() {
            return;
        }
        let held: HashSet<[u8; 32]> =
            tracker.history().entries().map(|e| e.content().digest()).collect();
        if let Some(running) = self.running.take_if(|r| !held.contains(&r.want.digest)) {
            out.push(Do::Cancel { fetch: running.fetch });
        }
        self.wants.retain(|w| held.contains(&w.digest));
    }

    /// Starts the next fetch, if none runs: one waited for by a paste, then the clipboard's
    /// entry's, then the smallest.
    fn pump(&mut self, tracker: &Tracker, out: &mut Vec<Do>) {
        self.settle(tracker, out);
        if self.running.is_some() {
            return;
        }
        let current = self.state.and_then(|s| s.digest);
        let next = self
            .wants
            .iter()
            .enumerate()
            .filter(|(_, w)| self.sessions.contains_key(&w.key))
            .min_by_key(|(_, w)| (!w.urgent, Some(w.digest) != current, w.size))
            .map(|(i, _)| i);
        let Some(want) = next.map(|i| self.wants.swap_remove(i)) else { return };
        let Some(&session) = self.sessions.get(&want.key) else { return };
        let Ok(name) = Name::new(&want.name.to_latin1()) else { return };
        let fetch = self.next_fetch;
        self.next_fetch = self.next_fetch.wrapping_add(1);
        out.push(Do::Fetch {
            key: want.key,
            session,
            fetch,
            digest: want.digest,
            name,
            size: want.size,
        });
        self.running = Some(Running { fetch, want });
    }
}
