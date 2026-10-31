//! Clipboard sharing's streams between two links on loopback (docs/sharing.md, "Streams"), some
//! through a relay that holds traffic to a bandwidth, as a LAN or Wi-Fi link does, to show that a
//! large fetch never holds input up.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use jackstay_net::peers::{self, Peer};
use jackstay_net::{
    Address, Clip, Config, Event, FetchFailed, Handle, Identity, Input, PeerState, PublicKey,
    Timings, start,
};
use jackstay_proto::link::Message;
use jackstay_proto::sharing::{Announce, Name, Target};

#[track_caller]
fn ok<T, E: std::fmt::Display>(r: Result<T, E>) -> T {
    r.unwrap_or_else(|e| panic!("{e}"))
}

/// A port with the next one also free, from a range of this process's own.
fn ports() -> u16 {
    const SPAN: u32 = 300;
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let base = 15_000 + (std::process::id() % 150) * SPAN;
    loop {
        let n = NEXT.fetch_add(2, Ordering::Relaxed);
        assert!(n < SPAN, "this process's ports are used up");
        let port = u16::try_from(base + n).unwrap_or_else(|e| panic!("{e}"));
        if UdpSocket::bind(("0.0.0.0", port)).is_ok()
            && UdpSocket::bind(("0.0.0.0", port + 1)).is_ok()
        {
            return port;
        }
    }
}

fn address(port: u16) -> Address {
    Address::parse(&format!("127.0.0.1:{port}")).unwrap_or_else(|| panic!("address"))
}

struct Machine {
    dir: std::path::PathBuf,
    port: u16,
    key: PublicKey,
}

impl Machine {
    fn new(name: &str) -> Machine {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("clip-{}-{n}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        ok(std::fs::create_dir_all(&dir));
        let key = ok(Identity::load_or_create(&dir.join("identity"))).public();
        Machine { dir, port: ports(), key }
    }

    fn knows_at(&self, other: &Machine, port: u16) {
        let all = vec![Peer { key: other.key, address: address(port), name: "peer".to_owned() }];
        ok(peers::save(&self.dir.join("peers"), &all));
    }

    fn start(&self) -> Link {
        let (tx, events) = channel();
        let (input_tx, inputs) = channel();
        let config = Config {
            state_dir: self.dir.clone(),
            port: self.port,
            name: "test".to_owned(),
            timings: Timings::default(),
        };
        let handle = ok(start(
            config,
            move |e| {
                let _ = tx.send(e);
            },
            move |i| {
                let _ = input_tx.send(i);
            },
        ));
        Link { handle, events, inputs, up: std::cell::Cell::new(None) }
    }
}

struct Link {
    handle: Handle,
    events: Receiver<Event>,
    inputs: Receiver<Input>,
    /// The session up now, as the events read so far say.
    up: std::cell::Cell<Option<u64>>,
}

impl Link {
    /// The next event within `within`, noting sessions up and down.
    fn next(&self, within: Duration) -> Result<Event, RecvTimeoutError> {
        let e = self.events.recv_timeout(within)?;
        match &e {
            Event::Clip(Clip::Up { session, .. }) => self.up.set(Some(*session)),
            Event::Clip(Clip::Down { session, .. }) if self.up.get() == Some(*session) => {
                self.up.set(None);
            }
            _ => {}
        }
        Ok(e)
    }

    /// The next clipboard report `pick` takes, within `within`.
    fn clip<T>(&self, within: Duration, what: &str, mut pick: impl FnMut(Clip) -> Option<T>) -> T {
        let until = Instant::now() + within;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            match self.next(left) {
                Ok(Event::Clip(c)) => {
                    if let Some(t) = pick(c) {
                        return t;
                    }
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => panic!("no {what} within {within:?}"),
                Err(RecvTimeoutError::Disconnected) => panic!("link ended waiting for {what}"),
            }
        }
    }

    /// Whether no clipboard report `pick` takes comes within `within`.
    fn no_clip(&self, within: Duration, mut pick: impl FnMut(&Clip) -> bool) -> bool {
        let until = Instant::now() + within;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            match self.next(left) {
                Ok(Event::Clip(c)) if pick(&c) => return false,
                Ok(_) => {}
                Err(_) => return true,
            }
        }
    }

    fn peer_in(&self, key: PublicKey, state: PeerState, within: Duration) {
        let until = Instant::now() + within;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            match self.next(left) {
                Ok(Event::Status(s))
                    if s.peers.iter().any(|p| p.key == key && p.state == state) =>
                {
                    return;
                }
                Ok(_) => {}
                Err(e) => panic!("not {state:?} within {within:?}: {e}"),
            }
        }
    }
}

const SOON: Duration = Duration::from_secs(5);

/// Two links, A and B, each knowing the other at `a_at` and `b_at`, settled on one session:
/// the session each says is up last.
fn linked(a: &Machine, b: &Machine, a_at: u16, b_at: u16) -> (Link, u64, Link, u64) {
    a.knows_at(b, b_at);
    b.knows_at(a, a_at);
    let (la, lb) = (a.start(), b.start());
    la.peer_in(b.key, PeerState::Connected, SOON);
    lb.peer_in(a.key, PeerState::Connected, SOON);
    // Both may dial at once; one session is kept within the dial deadline and backoff.
    let timings = Timings::default();
    std::thread::sleep(timings.backoff_max + timings.dial_deadline + Duration::from_millis(500));
    let last = |l: &Link| {
        while l.next(Duration::ZERO).is_ok() {}
        l.up.get().unwrap_or_else(|| panic!("no session up"))
    };
    let (sa, sb) = (last(&la), last(&lb));
    (la, sa, lb, sb)
}

fn direct() -> (Machine, Link, u64, Machine, Link, u64) {
    let (a, b) = (Machine::new("a"), Machine::new("b"));
    let (la, sa, lb, sb) = linked(&a, &b, a.port, b.port);
    (a, la, sa, b, lb, sb)
}

fn name(s: &str) -> Name {
    ok(Name::new(s.as_bytes()))
}

fn announce(tick: u64, inline: &[u8]) -> Message {
    Message::Announce(Box::new(Announce {
        tick,
        digest: [7; 32],
        replaces: None,
        copied_at_ms: 1,
        targets: vec![Target {
            name: name("UTF8_STRING"),
            type_name: name("UTF8_STRING"),
            format: 8,
            size: u64::try_from(inline.len()).unwrap_or(0),
            sha256: [1; 32],
            inline: Some(inline.to_vec()),
        }],
    }))
}

#[test]
fn shared_messages_arrive_each_on_its_own_stream() {
    let (a, la, sa, b, lb, _sb) = direct();
    let big = vec![9u8; 60 * 1024];
    let sent = vec![
        announce(1, b"hello"),
        announce(2, &big),
        Message::Emptied { tick: 3 },
        Message::Forget { tick: 4, digests: vec![[2; 32]; 1000] },
    ];
    for m in &sent {
        la.handle.share(b.key, sa, m.clone());
    }
    // Not one of clipboard sharing's: never sent on a stream of its own.
    la.handle.share(b.key, sa, Message::Motion { span: 1, dx: 1, dy: 1 });
    let mut got = Vec::new();
    for _ in 0..sent.len() {
        got.push(lb.clip(SOON, "a shared message", |c| match c {
            Clip::Received { key, message, .. } if key == a.key => Some(message),
            _ => None,
        }));
    }
    // Streams are not ordered between them; each arrives whole.
    for m in &sent {
        assert!(got.contains(m), "{m:?} missing");
    }
    assert!(lb.no_clip(Duration::from_millis(500), |c| matches!(c, Clip::Received { .. })));
    // Sent on a session no longer kept: dropped.
    la.handle.share(b.key, sa + 1000, Message::Emptied { tick: 5 });
    assert!(lb.no_clip(Duration::from_millis(500), |c| matches!(c, Clip::Received { .. })));
}

/// B answers each of A's fetches with what `answer` gives for its name.
fn answering(lb: &Link, answer: impl Fn(&Name) -> Option<Arc<[u8]>>) {
    let (request, name) = lb.clip(SOON, "a fetch", |c| match c {
        Clip::Wanted { request, name, .. } => Some((request, name)),
        _ => None,
    });
    lb.handle.answer(request, answer(&name));
}

fn pattern(len: usize) -> Arc<[u8]> {
    (0..len).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect::<Vec<u8>>().into()
}

#[test]
fn a_fetch_brings_the_whole_target_or_says_why_not() {
    let (_a, la, sa, b, lb, _sb) = direct();
    let image = pattern(3 * 1024 * 1024 + 7);
    let size = u64::try_from(image.len()).unwrap_or(0);
    la.handle.fetch(b.key, sa, 1, [7; 32], name("image/png"), size);
    answering(&lb, |n| (n == &name("image/png")).then(|| Arc::clone(&image)));
    let fetched = la.clip(SOON, "the fetch", |c| match c {
        Clip::Fetched { fetch: 1, result } => Some(result),
        _ => None,
    });
    assert_eq!(fetched.as_deref(), Ok(&image[..]));
    // Not held.
    la.handle.fetch(b.key, sa, 2, [7; 32], name("image/png"), size);
    answering(&lb, |_| None);
    let got = la.clip(SOON, "the fetch", |c| match c {
        Clip::Fetched { fetch: 2, result } => Some(result),
        _ => None,
    });
    assert_eq!(got, Err(FetchFailed::NotHeld));
    // Fewer bytes than the size, and more: never taken as the target.
    for (fetch, len) in [(3, image.len() - 1), (4, image.len() + 1)] {
        la.handle.fetch(b.key, sa, fetch, [7; 32], name("image/png"), size);
        answering(&lb, |_| Some(pattern(len)));
        let got = la.clip(SOON, "the fetch", |c| match c {
            Clip::Fetched { fetch: f, result } if f == fetch => Some(result),
            _ => None,
        });
        assert_eq!(got, Err(FetchFailed::Wrong), "{len} bytes for {size}");
    }
    // On a session that is not the one kept: lost at once.
    la.handle.fetch(b.key, sa + 1000, 5, [7; 32], name("image/png"), size);
    let got = la.clip(SOON, "the fetch", |c| match c {
        Clip::Fetched { fetch: 5, result } => Some(result),
        _ => None,
    });
    assert_eq!(got, Err(FetchFailed::Lost));
}

#[test]
fn an_unanswered_fetch_is_not_held_after_five_seconds() {
    let (_a, la, sa, b, lb, _sb) = direct();
    la.handle.fetch(b.key, sa, 1, [7; 32], name("TEXT"), 4);
    lb.clip(SOON, "a fetch", |c| matches!(c, Clip::Wanted { .. }).then_some(()));
    let asked = Instant::now();
    let got = la.clip(Duration::from_secs(8), "the fetch", |c| match c {
        Clip::Fetched { fetch: 1, result } => Some(result),
        _ => None,
    });
    assert_eq!(got, Err(FetchFailed::NotHeld));
    let took = asked.elapsed();
    assert!(took >= Duration::from_millis(4900) && took < Duration::from_secs(7), "{took:?}");
}

#[test]
fn a_cancelled_fetch_is_never_reported() {
    let (_a, la, sa, b, lb, _sb) = direct();
    la.handle.fetch(b.key, sa, 1, [7; 32], name("image/png"), 1024);
    let request = lb.clip(SOON, "a fetch", |c| match c {
        Clip::Wanted { request, .. } => Some(request),
        _ => None,
    });
    la.handle.cancel_fetch(1);
    lb.handle.answer(request, Some(pattern(1024)));
    assert!(la.no_clip(Duration::from_secs(1), |c| matches!(c, Clip::Fetched { .. })));
}

#[test]
fn a_fetch_from_a_peer_that_goes_is_lost() {
    let (_a, la, sa, b, lb, _sb) = direct();
    la.handle.fetch(b.key, sa, 1, [7; 32], name("image/png"), 1024);
    lb.clip(SOON, "a fetch", |c| matches!(c, Clip::Wanted { .. }).then_some(()));
    // B shuts down without answering: its sessions say goodbye.
    drop(lb);
    let got = la.clip(SOON, "the fetch", |c| match c {
        Clip::Fetched { fetch: 1, result } => Some(result),
        _ => None,
    });
    assert_eq!(got, Err(FetchFailed::Lost));
    // The session's end, reported before or after the fetch's.
    while la.up.get().is_some() {
        la.next(SOON).unwrap_or_else(|e| panic!("no session's end: {e}"));
    }
}

/// Datagrams held to `bytes_per_second`, at most `queue` behind, as a link's bottleneck holds
/// them: one lane, from one socket to an address.
struct Lane {
    pending: VecDeque<(Instant, Vec<u8>)>,
    free_at: Instant,
}

/// Forwards a machine's datagrams to another's port and back, both ways held to a bandwidth:
/// a machine told the peer is at `port` reaches it through here.
struct Relay {
    port: u16,
    stop: Arc<AtomicBool>,
}

impl Relay {
    fn to(target: u16, bytes_per_second: f64, queue: Duration) -> Relay {
        let outer = ok(UdpSocket::bind("127.0.0.1:0"));
        let inner = ok(UdpSocket::bind("127.0.0.1:0"));
        let port = ok(outer.local_addr()).port();
        let stop = Arc::new(AtomicBool::new(false));
        let client: Arc<Mutex<Option<SocketAddr>>> = Arc::new(Mutex::new(None));
        let target = SocketAddr::from(([127, 0, 0, 1], target));
        let lanes = [
            (ok(outer.try_clone()), ok(inner.try_clone()), true),
            (ok(inner.try_clone()), ok(outer.try_clone()), false),
        ];
        for (from, out, toward_target) in lanes {
            let (stop, client) = (Arc::clone(&stop), Arc::clone(&client));
            std::thread::spawn(move || {
                let mut lane = Lane { pending: VecDeque::new(), free_at: Instant::now() };
                let mut buf = vec![0u8; 65_536];
                while !stop.load(Ordering::Relaxed) {
                    let wait = lane.pending.front().map_or(Duration::from_millis(20), |(at, _)| {
                        at.saturating_duration_since(Instant::now()).max(Duration::from_micros(50))
                    });
                    let _ = from.set_read_timeout(Some(wait));
                    if let Ok((n, src)) = from.recv_from(&mut buf) {
                        if toward_target {
                            *client.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                                Some(src);
                        }
                        let now = Instant::now();
                        let start = lane.free_at.max(now);
                        if start.saturating_duration_since(now) <= queue {
                            let len = u32::try_from(n).unwrap_or(u32::MAX);
                            lane.free_at =
                                start + Duration::from_secs_f64(f64::from(len) / bytes_per_second);
                            lane.pending.push_back((
                                lane.free_at,
                                buf.get(..n).unwrap_or_default().to_vec(),
                            ));
                        }
                    }
                    let now = Instant::now();
                    while lane.pending.front().is_some_and(|(at, _)| *at <= now) {
                        let Some((_, d)) = lane.pending.pop_front() else { break };
                        let to = if toward_target {
                            Some(target)
                        } else {
                            *client.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
                        };
                        if let Some(to) = to {
                            let _ = out.send_to(&d, to);
                        }
                    }
                }
            });
        }
        Relay { port, stop }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Input latency under load (docs/sharing.md, "Streams"): B fetches 32 MiB from A over a 100
/// Mbit/s link with 50 ms of queue, while A drives B's pointer; motion waits behind at most the
/// fetch's window, never the whole queue.
#[test]
fn motion_crosses_a_slow_link_quickly_during_a_large_fetch() {
    let (a, b) = (Machine::new("a"), Machine::new("b"));
    let (bps, queue) = (100e6 / 8.0, Duration::from_millis(50));
    let (to_a, to_b) = (Relay::to(a.port, bps, queue), Relay::to(b.port, bps, queue));
    let (la, sa, lb, sb) = linked(&a, &b, to_a.port, to_b.port);
    // Input flows over A's session: B reports it to input sharing.
    let mut kept = None;
    while let Ok(i) = lb.inputs.try_recv() {
        if let Input::Kept { session, .. } = i {
            kept = Some(session);
        }
    }
    assert!(kept.is_some(), "no session kept for input");
    la.handle.set_active(b.key, true);
    let image = pattern(32 * 1024 * 1024);
    let size = u64::try_from(image.len()).unwrap_or(0);
    lb.handle.fetch(a.key, sb, 1, [7; 32], name("image/png"), size);
    answering(&la, |_| Some(Arc::clone(&image)));
    let fetching = Instant::now();
    // Motion every 5 ms while the fetch runs, each timed to its arrival.
    let mut sent_at: HashMap<i32, Instant> = HashMap::new();
    let mut took = Vec::new();
    let mut done = false;
    let mut n: i32 = 0;
    let mut seen = HashSet::new();
    while !done {
        n += 1;
        sent_at.insert(n, Instant::now());
        la.handle.send(b.key, sa, Message::Motion { span: 1, dx: n, dy: 0 });
        std::thread::sleep(Duration::from_millis(5));
        while let Ok(i) = lb.inputs.try_recv() {
            if let Input::Message { message: Message::Motion { dx, .. }, .. } = i
                && seen.insert(dx)
                && let Some(at) = sent_at.get(&dx)
                && !done
            {
                took.push(at.elapsed());
            }
        }
        if !done
            && let Ok(Event::Clip(Clip::Fetched { fetch: 1, result })) = lb.next(Duration::ZERO)
        {
            assert_eq!(result.as_deref().map(<[u8]>::len), Ok(image.len()));
            done = true;
            eprintln!("32 MiB fetched in {:?}", fetching.elapsed());
        }
        assert!(fetching.elapsed() < Duration::from_secs(30), "the fetch took too long");
    }
    assert!(took.len() >= 100, "only {} motions timed", took.len());
    took.sort_unstable();
    let at = |q: usize| took[(took.len() - 1) * q / 100];
    let (median, p99) = (at(50), at(99));
    eprintln!("motion during the fetch: median {median:?}, p99 {p99:?}, {} samples", took.len());
    // The 256 KiB the receiver lets the fetch have unacknowledged queue at the bottleneck: 21 ms
    // at 100 Mbit/s, 2 ms at a gigabit (docs/sharing.md, "Streams").
    assert!(median < Duration::from_millis(30), "median {median:?}");
    assert!(p99 < Duration::from_millis(50), "p99 {p99:?}");
}
