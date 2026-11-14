//! Clipboard sharing between machines (docs/sharing.md), each a tracker and its sharing, the link
//! between them a queue of messages delivered when a test says, so every order the link's
//! unordered streams allow can be tried.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jackstay_clip::{
    Content, EntryId, Format, History, Kept, Limits, Origin, Source, Target, TargetData, TargetName,
};
use jackstay_net::{Clip, FetchFailed, PublicKey};
use jackstay_proto::link::Message;
use jackstay_proto::sharing::Name;
use jackstay_selection::Report;
use jackstayd::sharing::{Do, Sharing, raw_key};
use jackstayd::tracker::Tracker;

const SPKI: [u8; 12] = [0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];
const BIG: usize = 100 * 1024;

fn key(n: u8) -> PublicKey {
    let mut spki = SPKI.to_vec();
    spki.extend([n; 32]);
    PublicKey::from_spki(&spki).unwrap_or_else(|| panic!("a key"))
}

fn name(s: &str) -> TargetName {
    TargetName::from_latin1(s.as_bytes()).unwrap_or_else(|e| panic!("{e}"))
}

fn kept(n: &str, bytes: &[u8]) -> Target {
    let k = Kept::new(name(n), Format::Bits8, bytes.to_vec()).unwrap_or_else(|e| panic!("{e}"));
    Target { name: name(n), data: TargetData::Kept(k) }
}

fn content(targets: Vec<Target>) -> Arc<Content> {
    Arc::new(Content::new(targets).unwrap_or_else(|e| panic!("{e}")))
}

fn text(s: &str) -> Arc<Content> {
    content(vec![kept("UTF8_STRING", s.as_bytes())])
}

/// Text and an image too large to travel inside the announcement.
fn with_image(s: &str, fill: u8) -> Arc<Content> {
    content(vec![kept("UTF8_STRING", s.as_bytes()), kept("image/png", &vec![fill; BIG])])
}

/// One machine: its history, its sharing, and what it did.
struct Machine {
    key: PublicKey,
    tracker: Tracker,
    sharing: Sharing,
    /// The digest served as the clipboard, as the X server would see it.
    clipboard: Option<[u8; 32]>,
    /// Messages sent, to whom, not yet delivered.
    outbox: Vec<(PublicKey, Message)>,
    /// Fetches asked for and not yet answered: id, from whom, digest, target.
    fetches: Vec<(u64, PublicKey, [u8; 32], Name)>,
    cancelled: Vec<u64>,
    claims: usize,
    updates: usize,
    answers: Vec<(u64, Option<Arc<[u8]>>)>,
    next_capture: u64,
    now: Instant,
    wall: u64,
}

impl Machine {
    fn new(n: u8, start: Instant) -> Machine {
        let limits = Limits {
            max_entries: NonZeroUsize::new(10).unwrap_or(NonZeroUsize::MIN),
            max_bytes: 1 << 30,
        };
        let tracker = Tracker::new(History::new(limits), 5, false);
        let sharing = Sharing::new(key(n), &tracker, 64 << 20);
        Machine {
            key: key(n),
            tracker,
            sharing,
            clipboard: None,
            outbox: Vec::new(),
            fetches: Vec::new(),
            cancelled: Vec::new(),
            claims: 0,
            updates: 0,
            answers: Vec::new(),
            next_capture: 1,
            now: start,
            wall: 1_000_000,
        }
    }

    fn act(&mut self, dos: Vec<Do>) {
        for d in dos {
            match d {
                Do::Share { key, message, .. } => self.outbox.push((key, message)),
                Do::Fetch { key, fetch, digest, name, .. } => {
                    self.fetches.push((fetch, key, digest, name));
                }
                Do::Cancel { fetch } => {
                    self.cancelled.push(fetch);
                    self.fetches.retain(|f| f.0 != fetch);
                }
                Do::Answer { request, bytes } => self.answers.push((request, bytes)),
                Do::Serve(id) => {
                    self.clipboard = self.tracker.history().get(id).map(|e| e.content().digest());
                    self.claims += 1;
                    self.tracker.served(id, true);
                }
                Do::Update(id) => {
                    self.clipboard = self.tracker.history().get(id).map(|e| e.content().digest());
                    self.updates += 1;
                    self.tracker.served(id, true);
                }
                Do::Empty => {
                    self.clipboard = None;
                    self.tracker.emptied();
                }
                Do::Save => {}
            }
        }
    }

    fn reports(&mut self, reports: &[Report]) {
        for r in reports {
            self.tracker.on_report(r, self.wall);
        }
        let dos = self.sharing.reports(reports, &mut self.tracker, self.now, self.wall);
        self.act(dos);
    }

    /// An application here copies `c`, captured whole.
    fn copy(&mut self, c: &Arc<Content>) {
        let capture = self.start();
        self.captured(capture, c, true);
    }

    fn start(&mut self) -> u64 {
        let capture = self.next_capture;
        self.next_capture += 1;
        self.reports(&[Report::CaptureStarted { capture }]);
        capture
    }

    fn captured(&mut self, capture: u64, c: &Arc<Content>, complete: bool) {
        let source = Some("Firefox".to_owned());
        let r = Report::Captured { capture, content: Arc::clone(c), source, complete };
        if complete {
            self.clipboard = Some(c.digest());
        }
        self.reports(&[r]);
    }

    fn clip(&mut self, event: Clip) {
        let dos = self.sharing.clip(event, &mut self.tracker, &|_| "peer".to_owned(), self.wall);
        self.act(dos);
    }

    fn up(&mut self, peer: &Machine) {
        self.clip(Clip::Up { key: peer.key, session: 1 });
    }

    fn later(&mut self, ms: u64) {
        self.now += Duration::from_millis(ms);
        self.wall += ms;
        let dos = self.sharing.tick(&mut self.tracker, self.now);
        self.act(dos);
    }

    fn texts(&self) -> Vec<String> {
        self.tracker
            .history()
            .entries()
            .map(|e| {
                let k = e.content().kept("UTF8_STRING").map(|k| k.bytes().to_vec());
                String::from_utf8_lossy(&k.unwrap_or_default()).into_owned()
            })
            .collect()
    }

    fn held(&self, c: &Content) -> Option<EntryId> {
        self.tracker.history().find_digest(&c.digest())
    }

    fn announcements(&self) -> usize {
        self.outbox.iter().filter(|(_, m)| matches!(m, Message::Announce(_))).count()
    }
}

/// Two machines, linked.
fn pair() -> (Machine, Machine) {
    let start = Instant::now();
    let (mut a, mut b) = (Machine::new(1, start), Machine::new(2, start));
    a.up(&b);
    b.up(&a);
    (a, b)
}

/// Delivers what `from` sent `to`, in the order sent; what it sent others stays.
fn deliver(from: &mut Machine, to: &mut Machine) {
    let (messages, others): (Vec<_>, Vec<_>) =
        std::mem::take(&mut from.outbox).into_iter().partition(|(k, _)| *k == to.key);
    from.outbox = others;
    for (_, message) in messages {
        to.clip(Clip::Received { key: from.key, session: 1, message });
    }
}

/// Delivers both ways until nothing is left.
fn settle(a: &mut Machine, b: &mut Machine) {
    while a.outbox.iter().any(|(k, _)| *k == b.key) || b.outbox.iter().any(|(k, _)| *k == a.key) {
        deliver(a, b);
        deliver(b, a);
    }
}

/// `to`'s fetches answered by `from`, until none is left.
fn serve_fetches(from: &mut Machine, to: &mut Machine) {
    while let Some((fetch, _, digest, name)) = to.fetches.first().cloned() {
        to.fetches.remove(0);
        from.clip(Clip::Wanted { key: to.key, request: fetch, digest, name });
        let (_, bytes) = from.answers.pop().unwrap_or_else(|| panic!("no answer"));
        to.clip(Clip::Fetched { fetch, result: bytes.ok_or(FetchFailed::NotHeld) });
    }
}

#[test]
fn a_copy_on_one_machine_is_the_clipboard_on_the_other() {
    let (mut a, mut b) = pair();
    a.copy(&text("hello"));
    assert_eq!(a.announcements(), 1);
    settle(&mut a, &mut b);
    assert_eq!(b.clipboard, Some(text("hello").digest()));
    assert_eq!(b.texts(), ["hello"]);
    let id = b.held(&text("hello")).unwrap_or_else(|| panic!("held"));
    let entry = b.tracker.history().get(id).unwrap_or_else(|| panic!("entry"));
    assert_eq!(entry.source(), &Source::Peer { key: raw_key(&a.key), name: "peer".into() });
    assert_eq!(b.tracker.current(), Some(id));
    // Text travelled inside the announcement: nothing to fetch.
    assert_eq!(b.fetches.len(), 0, "{:?}", b.fetches);
    // And back.
    b.later(10);
    b.copy(&text("reply"));
    settle(&mut a, &mut b);
    assert_eq!(a.clipboard, Some(text("reply").digest()));
    assert_eq!(a.texts(), ["reply", "hello"]);
}

#[test]
fn a_large_target_is_fetched_and_served_in_place() {
    let (mut a, mut b) = pair();
    let shot = with_image("shot", 7);
    a.copy(&shot);
    settle(&mut a, &mut b);
    // Served at once, the image still to come; then fetched, and served in place.
    assert_eq!(b.clipboard, Some(shot.digest()));
    assert_eq!(b.claims, 1);
    assert_eq!(b.fetches.len(), 1);
    serve_fetches(&mut a, &mut b);
    assert_eq!((b.claims, b.updates), (1, 1));
    let id = b.held(&shot).unwrap_or_else(|| panic!("held"));
    let entry = b.tracker.history().get(id).unwrap_or_else(|| panic!("entry"));
    assert_eq!(entry.content().kept("image/png").map(|k| k.bytes().len()), Some(BIG));
}

#[test]
fn a_target_already_held_is_not_fetched() {
    let (mut a, mut b) = pair();
    let shot = with_image("shot", 7);
    b.copy(&shot);
    settle(&mut a, &mut b);
    serve_fetches(&mut b, &mut a);
    // The same image with other text: its bytes are already here.
    a.later(10);
    a.copy(&with_image("again", 7));
    settle(&mut a, &mut b);
    assert_eq!(b.fetches.len(), 0, "{:?}", b.fetches);
    assert_eq!(b.clipboard, Some(with_image("again", 7).digest()));
}

#[test]
fn copies_on_both_at_once_end_as_one_clipboard() {
    let (mut a, mut b) = pair();
    // The same wall-clock millisecond: the keys decide, the same way on both.
    a.copy(&text("from a"));
    b.copy(&text("from b"));
    settle(&mut a, &mut b);
    assert_eq!(a.clipboard, b.clipboard);
    assert_eq!(a.clipboard, Some(text("from b").digest()));
    assert_eq!(a.texts(), b.texts());
}

#[test]
fn the_later_copy_wins_after_the_link_comes_back() {
    let (mut a, mut b) = pair();
    // Apart: each copies, b later.
    a.clip(Clip::Down { key: b.key, session: 1 });
    b.clip(Clip::Down { key: a.key, session: 1 });
    a.copy(&text("earlier"));
    b.later(5_000);
    b.copy(&text("later"));
    assert!(a.outbox.is_empty() && b.outbox.is_empty());
    // Back: each replays its clipboard.
    a.clip(Clip::Up { key: b.key, session: 2 });
    b.clip(Clip::Up { key: a.key, session: 2 });
    settle(&mut a, &mut b);
    assert_eq!(a.clipboard, Some(text("later").digest()));
    assert_eq!(b.clipboard, Some(text("later").digest()));
    // The older copy is history on both, behind it.
    assert_eq!(a.texts(), ["later", "earlier"]);
    assert_eq!(b.texts(), ["later", "earlier"]);
}

#[test]
fn a_reconnect_with_nothing_new_claims_nothing() {
    let (mut a, mut b) = pair();
    a.copy(&text("same"));
    settle(&mut a, &mut b);
    assert_eq!((a.claims, b.claims), (0, 1));
    // Each replays its clipboard, the same change: no claim, so applications see nothing.
    for (m, peer) in [(&mut a, key(2)), (&mut b, key(1))] {
        m.clip(Clip::Down { key: peer, session: 1 });
        m.clip(Clip::Up { key: peer, session: 2 });
    }
    settle(&mut a, &mut b);
    assert_eq!((a.claims, b.claims), (0, 1));
    assert_eq!(a.clipboard, b.clipboard);
}

#[test]
fn a_copy_here_still_being_captured_is_never_abandoned() {
    let (mut a, mut b) = pair();
    // b's owner is slow: the capture started, nothing yet.
    let capture = b.start();
    a.later(100);
    a.copy(&text("newer, from a"));
    deliver(&mut a, &mut b);
    assert_eq!(b.claims, 0, "no claim over a capture in flight");
    // b's capture ends, its content later than a's copy but its owner change earlier: announced,
    // and a's newer change applied after it.
    b.later(200);
    b.captured(capture, &text("slow"), true);
    assert_eq!(b.clipboard, Some(text("newer, from a").digest()));
    settle(&mut a, &mut b);
    assert_eq!(a.clipboard, Some(text("newer, from a").digest()));
    assert_eq!(a.texts(), ["newer, from a", "slow"]);

    // The other way: b's capture started after a's copy, and wins.
    let (mut a, mut b) = pair();
    a.copy(&text("older, from a"));
    b.later(100);
    let capture = b.start();
    deliver(&mut a, &mut b);
    b.captured(capture, &text("b's"), true);
    assert_eq!(b.clipboard, Some(text("b's").digest()));
    settle(&mut a, &mut b);
    assert_eq!(a.clipboard, Some(text("b's").digest()));
}

#[test]
fn a_change_waiting_for_a_capture_that_fails_is_applied() {
    let (mut a, mut b) = pair();
    let capture = b.start();
    a.later(100);
    a.copy(&text("from a"));
    deliver(&mut a, &mut b);
    assert_eq!(b.claims, 0);
    // The owner died before answering: nothing captured, and a's change goes ahead.
    b.reports(&[Report::CaptureFailed { capture }]);
    assert_eq!(b.clipboard, Some(text("from a").digest()));
    assert_eq!(b.announcements(), 0);
}

#[test]
fn a_delete_deletes_everywhere_pinned_or_not() {
    let (mut a, mut b) = pair();
    a.copy(&text("password"));
    settle(&mut a, &mut b);
    let on_b = b.held(&text("password")).unwrap_or_else(|| panic!("held"));
    assert!(b.tracker.pin(on_b, true).is_ok());
    let tick = a.tracker.next_tick(a.wall);
    let dos = a.sharing.deleted(&mut a.tracker, &[(text("password").digest(), Origin::Here)], tick);
    a.act(dos);
    settle(&mut a, &mut b);
    assert_eq!(b.held(&text("password")), None);
    // It was b's clipboard: emptied.
    assert_eq!(b.clipboard, None);
}

#[test]
fn a_delete_that_overtakes_its_announcement_still_wins() {
    let (mut a, mut b) = pair();
    a.copy(&text("secret"));
    let tick = a.tracker.next_tick(a.wall);
    let dos = a.sharing.deleted(&mut a.tracker, &[(text("secret").digest(), Origin::Here)], tick);
    a.act(dos);
    // The Forget arrives first.
    let mut sent = std::mem::take(&mut a.outbox);
    sent.reverse();
    a.outbox = sent;
    deliver(&mut a, &mut b);
    assert_eq!(b.held(&text("secret")), None);
    assert_eq!(b.clipboard, None);
    // Copied again later: a newer change, kept.
    a.later(10);
    a.copy(&text("secret"));
    settle(&mut a, &mut b);
    assert!(b.held(&text("secret")).is_some());
}

#[test]
fn a_delete_while_away_arrives_on_return() {
    let (mut a, mut b) = pair();
    a.copy(&text("one"));
    a.later(10);
    a.copy(&text("two"));
    settle(&mut a, &mut b);
    b.clip(Clip::Down { key: a.key, session: 1 });
    a.clip(Clip::Down { key: b.key, session: 1 });
    let tick = a.tracker.next_tick(a.wall);
    let dos = a.sharing.deleted(&mut a.tracker, &[(text("one").digest(), Origin::Here)], tick);
    a.act(dos);
    assert_eq!(a.outbox.len(), 0, "{:?}", a.outbox);
    a.clip(Clip::Up { key: b.key, session: 2 });
    b.clip(Clip::Up { key: a.key, session: 2 });
    settle(&mut a, &mut b);
    assert_eq!(b.texts(), ["two"]);
    // Replayed forgets are remembered: an old announcement of it is refused.
    assert!(b.tracker.history().was_forgotten(&text("one").digest(), tick));
}

#[test]
fn another_tool_copying_our_clipboard_is_no_change() {
    let (mut a, mut b) = pair();
    a.copy(&text("x"));
    settle(&mut a, &mut b);
    // A tool on b (CopyQ, a VM's agent) takes the clipboard with what it holds.
    b.copy(&text("x"));
    assert_eq!(b.announcements(), 0);
}

#[test]
fn too_many_copies_a_second_stop_announcing_for_a_while() {
    let (mut a, _b) = pair();
    for i in 0..11 {
        a.copy(&text(&format!("loop {i}")));
        a.later(50);
    }
    assert_eq!(a.announcements(), 10);
    assert!(a.sharing.paused(a.now));
    a.outbox.clear();
    a.later(10_000);
    assert!(!a.sharing.paused(a.now));
    a.copy(&text("calm"));
    assert_eq!(a.announcements(), 1);
}

#[test]
fn an_owner_giving_the_clipboard_up_empties_it_everywhere() {
    let (mut a, mut b) = pair();
    a.copy(&text("from the password manager"));
    settle(&mut a, &mut b);
    a.later(10);
    a.reports(&[Report::Emptied]);
    assert_eq!(a.tracker.current(), None);
    assert!(a.tracker.cleared(), "a restart puts nothing back");
    settle(&mut a, &mut b);
    assert_eq!(b.clipboard, None);
    // History keeps it, on both.
    assert_eq!(a.texts(), ["from the password manager"]);
    assert_eq!(b.texts(), ["from the password manager"]);
    assert!(b.tracker.cleared());
}

#[test]
fn a_reselect_goes_back_to_the_peer_an_entry_came_from() {
    let (mut a, mut b) = pair();
    a.copy(&text("first"));
    a.later(10);
    a.copy(&text("second"));
    settle(&mut a, &mut b);
    // b selects the first again: a moves it to the front and serves it.
    b.later(10);
    let id = b.held(&text("first")).unwrap_or_else(|| panic!("held"));
    let tick = b.tracker.next_tick(b.wall);
    assert!(b.tracker.select(id, tick).is_some());
    let dos = b.sharing.selected(&b.tracker, id);
    b.act(dos);
    settle(&mut a, &mut b);
    assert_eq!(a.clipboard, Some(text("first").digest()));
    assert_eq!(a.texts(), ["first", "second"]);
}

#[test]
fn a_capture_still_running_is_announced_early_then_completed_in_place() {
    let (mut a, mut b) = pair();
    let capture = a.start();
    a.captured(capture, &text("text first"), false);
    assert_eq!(a.announcements(), 0);
    a.later(1_000);
    assert_eq!(a.announcements(), 1);
    settle(&mut a, &mut b);
    assert_eq!(b.clipboard, Some(text("text first").digest()));
    // Complete: announced again, replacing the early one, served in place on b.
    let whole = with_image("text first", 3);
    a.captured(capture, &whole, true);
    settle(&mut a, &mut b);
    assert_eq!(b.clipboard, Some(whole.digest()));
    assert_eq!((b.claims, b.updates), (1, 1));
    assert_eq!(b.held(&text("text first")), None);
    assert_eq!(b.texts(), ["text first"]);
}

#[test]
fn a_paste_waiting_puts_its_fetch_first() {
    let (mut a, mut b) = pair();
    let older = with_image("older", 1);
    a.copy(&older);
    a.later(10);
    let newer = with_image("newer", 2);
    a.copy(&newer);
    deliver(&mut a, &mut b);
    // The first announcement's fetch is running; the clipboard's image waits behind it.
    assert_eq!(b.fetches.len(), 1);
    assert_eq!(b.fetches.first().map(|f| f.2), Some(older.digest()));
    b.reports(&[Report::Wanted { target: name("image/png") }]);
    assert_eq!(b.cancelled.len(), 1);
    assert_eq!(b.fetches.first().map(|f| f.2), Some(newer.digest()));
    serve_fetches(&mut a, &mut b);
    // Both fetched in the end.
    for c in [&older, &newer] {
        let id = b.held(c).unwrap_or_else(|| panic!("held"));
        let e = b.tracker.history().get(id).unwrap_or_else(|| panic!("entry"));
        assert!(e.content().kept("image/png").is_some());
    }
}

#[test]
fn an_entry_deleted_while_fetched_cancels_its_fetch() {
    let (mut a, mut b) = pair();
    let shot = with_image("shot", 9);
    a.copy(&shot);
    settle(&mut a, &mut b);
    let (fetch, ..) = b.fetches.first().cloned().unwrap_or_else(|| panic!("a fetch"));
    let tick = a.tracker.next_tick(a.wall);
    let dos = a.sharing.deleted(&mut a.tracker, &[(shot.digest(), Origin::Here)], tick);
    a.act(dos);
    settle(&mut a, &mut b);
    assert_eq!(b.cancelled, [fetch]);
    assert_eq!(b.fetches.len(), 0, "{:?}", b.fetches);
}

#[test]
fn a_failed_fetch_is_not_retried_until_a_paste_asks() {
    let (mut a, mut b) = pair();
    let shot = with_image("shot", 4);
    a.copy(&shot);
    settle(&mut a, &mut b);
    let (fetch, ..) = b.fetches.remove(0);
    b.clip(Clip::Fetched { fetch, result: Err(FetchFailed::NotHeld) });
    assert_eq!(b.fetches.len(), 0, "{:?}", b.fetches);
    b.clip(Clip::Down { key: a.key, session: 1 });
    b.clip(Clip::Up { key: a.key, session: 2 });
    assert_eq!(b.fetches.len(), 0, "{:?}", b.fetches);
    b.reports(&[Report::Wanted { target: name("image/png") }]);
    assert_eq!(b.fetches.len(), 1);
    // Bytes that do not match the hash are refused.
    let (fetch, ..) = b.fetches.remove(0);
    let wrong: Arc<[u8]> = vec![0u8; BIG].into();
    b.clip(Clip::Fetched { fetch, result: Ok(wrong) });
    let id = b.held(&shot).unwrap_or_else(|| panic!("held"));
    let e = b.tracker.history().get(id).unwrap_or_else(|| panic!("entry"));
    assert!(e.content().kept("image/png").is_none());
}

#[test]
fn a_lost_fetch_is_asked_for_again_on_the_next_session() {
    let (mut a, mut b) = pair();
    a.copy(&with_image("shot", 5));
    settle(&mut a, &mut b);
    let (fetch, ..) = b.fetches.remove(0);
    b.clip(Clip::Down { key: a.key, session: 1 });
    b.clip(Clip::Fetched { fetch, result: Err(FetchFailed::Lost) });
    assert_eq!(b.fetches.len(), 0, "{:?}", b.fetches);
    b.clip(Clip::Up { key: a.key, session: 2 });
    assert_eq!(b.fetches.len(), 1);
}

#[test]
fn announcements_that_lie_or_run_ahead_are_refused() {
    let (mut a, mut b) = pair();
    a.copy(&text("honest"));
    let Some((_, Message::Announce(mut lie))) = a.outbox.pop() else { panic!("an announcement") };
    lie.digest = [0; 32];
    b.clip(Clip::Received { key: a.key, session: 1, message: Message::Announce(lie.clone()) });
    assert_eq!(b.texts().len(), 0, "{:?}", b.texts());
    // Inline bytes not matching their hash.
    let mut wrong = lie.clone();
    wrong.digest = text("honest").digest();
    if let Some(t) = wrong.targets.first_mut() {
        t.inline = Some(b"hacked".to_vec());
    }
    b.clip(Clip::Received { key: a.key, session: 1, message: Message::Announce(wrong) });
    assert_eq!(b.texts().len(), 0, "{:?}", b.texts());
    // A day ahead of b's clock.
    let mut ahead = lie;
    ahead.digest = text("honest").digest();
    ahead.tick = b.wall + 24 * 60 * 60 * 1000 + 1;
    b.clip(Clip::Received { key: a.key, session: 1, message: Message::Announce(ahead.clone()) });
    assert_eq!(b.texts().len(), 0, "{:?}", b.texts());
    ahead.tick -= 1;
    b.clip(Clip::Received { key: a.key, session: 1, message: Message::Announce(ahead) });
    assert_eq!(b.texts(), ["honest"]);
}

#[test]
fn a_peer_asking_for_a_target_gets_its_bytes_or_not_held() {
    let (mut a, b) = pair();
    let shot = with_image("shot", 6);
    a.copy(&shot);
    let png = Name::new(b"image/png").unwrap_or_else(|e| panic!("{e:?}"));
    a.clip(Clip::Wanted { key: b.key, request: 1, digest: shot.digest(), name: png.clone() });
    a.clip(Clip::Wanted { key: b.key, request: 2, digest: [1; 32], name: png });
    assert_eq!(
        a.answers.first().map(|(r, b)| (*r, b.as_ref().map(|b| b.len()))),
        Some((1, Some(BIG)))
    );
    assert_eq!(a.answers.get(1).map(|(r, b)| (*r, b.is_none())), Some((2, true)));
}

/// Athena (1) between xiphos (2) and flapper (3), which are not paired with each other: the
/// owner's three machines (docs/sharing.md, "Who hears what").
fn three() -> (Machine, Machine, Machine) {
    let start = Instant::now();
    let (mut athena, mut xiphos, mut flapper) =
        (Machine::new(1, start), Machine::new(2, start), Machine::new(3, start));
    athena.up(&xiphos);
    athena.up(&flapper);
    xiphos.up(&athena);
    flapper.up(&athena);
    (athena, xiphos, flapper)
}

/// What `from` has queued for `to`.
fn queued_for<'a>(from: &'a Machine, to: &Machine) -> Vec<&'a Message> {
    from.outbox.iter().filter(|(k, _)| *k == to.key).map(|(_, m)| m).collect()
}

/// `m`'s session with `peer` ends, and a new one starts.
fn reconnect(m: &mut Machine, peer: &Machine, session: u64) {
    m.clip(Clip::Down { key: peer.key, session: session - 1 });
    m.clip(Clip::Up { key: peer.key, session });
}

#[test]
fn a_copy_from_one_peer_never_reaches_another() {
    let (mut athena, mut xiphos, mut flapper) = three();
    xiphos.copy(&text("work secret"));
    settle(&mut xiphos, &mut athena);
    assert_eq!(athena.clipboard, Some(text("work secret").digest()));
    assert!(queued_for(&athena, &flapper).is_empty(), "{:?}", athena.outbox);
    // Flapper wakes: athena sends its clipboard, which is xiphos's; it is not sent.
    reconnect(&mut athena, &flapper, 2);
    reconnect(&mut flapper, &athena, 2);
    assert!(queued_for(&athena, &flapper).is_empty(), "{:?}", athena.outbox);
    // Picked again on athena: back to xiphos only.
    athena.later(10);
    let id = athena.held(&text("work secret")).unwrap_or_else(|| panic!("held"));
    let tick = athena.tracker.next_tick(athena.wall);
    assert!(athena.tracker.select(id, tick).is_some());
    let dos = athena.sharing.selected(&athena.tracker, id);
    athena.act(dos);
    assert_eq!(queued_for(&athena, &xiphos).len(), 1);
    assert!(queued_for(&athena, &flapper).is_empty(), "{:?}", athena.outbox);
    settle(&mut athena, &mut flapper);
    assert!(flapper.texts().is_empty(), "{:?}", flapper.texts());
    // Xiphos's owner gives its clipboard up: athena empties, flapper hears nothing of it.
    settle(&mut athena, &mut xiphos);
    xiphos.later(10);
    xiphos.reports(&[Report::Emptied]);
    settle(&mut xiphos, &mut athena);
    assert_eq!(athena.clipboard, None);
    reconnect(&mut athena, &flapper, 3);
    assert!(queued_for(&athena, &flapper).is_empty(), "{:?}", athena.outbox);
    // A copy on athena reaches both, and flapper's reaches athena only.
    athena.later(10);
    athena.copy(&text("home"));
    settle(&mut athena, &mut flapper);
    settle(&mut athena, &mut xiphos);
    assert_eq!(
        (xiphos.clipboard, flapper.clipboard),
        (Some(text("home").digest()), Some(text("home").digest()))
    );
    flapper.later(20);
    flapper.copy(&text("laptop"));
    settle(&mut flapper, &mut athena);
    assert_eq!(athena.clipboard, Some(text("laptop").digest()));
    assert!(queued_for(&athena, &xiphos).is_empty(), "{:?}", athena.outbox);
    assert_eq!(flapper.texts(), ["laptop", "home"]);
    assert_eq!(xiphos.texts(), ["home", "work secret"]);
}

#[test]
fn a_deletion_reaches_only_the_peers_its_entry_may_have_reached() {
    let (mut athena, mut xiphos, mut flapper) = three();
    xiphos.copy(&text("work"));
    settle(&mut xiphos, &mut athena);
    athena.later(10);
    athena.copy(&text("home"));
    settle(&mut athena, &mut xiphos);
    settle(&mut athena, &mut flapper);
    let origin = |m: &Machine, c: &Arc<Content>| {
        let id = m.held(c).unwrap_or_else(|| panic!("held"));
        Origin::of(m.tracker.history().get(id).unwrap_or_else(|| panic!("entry")).source())
    };
    let gone = [
        (text("work").digest(), origin(&athena, &text("work"))),
        (text("home").digest(), origin(&athena, &text("home"))),
    ];
    assert_eq!(gone[0].1, Origin::Peer(raw_key(&xiphos.key)));
    let tick = athena.tracker.next_tick(athena.wall);
    let dos = athena.sharing.deleted(&mut athena.tracker, &gone, tick);
    athena.act(dos);
    let forgets = |m: &Machine, to: &Machine| -> Vec<[u8; 32]> {
        queued_for(m, to)
            .into_iter()
            .flat_map(|msg| match msg {
                Message::Forget { digests, .. } => digests.clone(),
                _ => Vec::new(),
            })
            .collect()
    };
    assert_eq!(forgets(&athena, &xiphos), [text("work").digest(), text("home").digest()]);
    assert_eq!(forgets(&athena, &flapper), [text("home").digest()]);
    settle(&mut athena, &mut xiphos);
    settle(&mut athena, &mut flapper);
    assert!(xiphos.texts().is_empty() && flapper.texts().is_empty());
    // Replayed on a new session, the same: what flapper may have held, and no more.
    reconnect(&mut athena, &flapper, 2);
    assert_eq!(forgets(&athena, &flapper), [text("home").digest()]);
    athena.outbox.clear();
    // A deletion xiphos sends of its own entry is remembered as xiphos's; one of an entry not
    // held here, as nobody's: neither goes on to flapper.
    xiphos.later(10);
    xiphos.copy(&text("more work"));
    settle(&mut xiphos, &mut athena);
    let tick = xiphos.tracker.next_tick(xiphos.wall);
    let ours = (text("more work").digest(), Origin::Here);
    let dos = xiphos.sharing.deleted(&mut xiphos.tracker, &[ours, ([9; 32], Origin::Here)], tick);
    xiphos.act(dos);
    settle(&mut xiphos, &mut athena);
    let remembered: Vec<Origin> =
        athena.tracker.history().forgotten().filter(|f| f.tick == tick).map(|f| f.origin).collect();
    assert_eq!(remembered, [Origin::Peer(raw_key(&xiphos.key)), Origin::Unknown]);
    reconnect(&mut athena, &flapper, 3);
    assert_eq!(forgets(&athena, &flapper), [text("home").digest()]);
}

#[test]
fn another_peers_entry_is_neither_fetched_from_nor_given_to_a_peer() {
    let (mut athena, mut xiphos, mut flapper) = three();
    let shot = with_image("work shot", 7);
    xiphos.copy(&shot);
    deliver(&mut xiphos, &mut athena);
    // Xiphos goes mid-fetch: the image is still to come from it, and from it only.
    let (fetch, from, ..) = athena.fetches.remove(0);
    assert_eq!(from, xiphos.key);
    athena.clip(Clip::Down { key: xiphos.key, session: 1 });
    athena.clip(Clip::Fetched { fetch, result: Err(FetchFailed::Lost) });
    // Flapper arrives: not asked for it.
    reconnect(&mut athena, &flapper, 2);
    assert!(athena.fetches.is_empty(), "{:?}", athena.fetches);
    // A paste on athena waits for it; flapper is not asked.
    athena.reports(&[Report::Wanted { target: name("image/png") }]);
    assert!(athena.fetches.is_empty(), "{:?}", athena.fetches);
    // Xiphos back: asked of it.
    athena.clip(Clip::Up { key: xiphos.key, session: 2 });
    assert_eq!(athena.fetches.iter().map(|f| f.1).collect::<Vec<_>>(), [xiphos.key]);
    // Flapper asking for its text by digest is told it is not held.
    let utf8 = Name::new(b"UTF8_STRING").unwrap_or_else(|e| panic!("{e:?}"));
    athena.clip(Clip::Wanted {
        key: flapper.key,
        request: 9,
        digest: shot.digest(),
        name: utf8.clone(),
    });
    assert_eq!(athena.answers.last().map(|(r, b)| (*r, b.is_none())), Some((9, true)));
    // Xiphos itself is answered.
    athena.clip(Clip::Wanted { key: xiphos.key, request: 10, digest: shot.digest(), name: utf8 });
    assert_eq!(athena.answers.last().map(|(r, b)| (*r, b.is_some())), Some((10, true)));
    let _ = &mut flapper;
}
