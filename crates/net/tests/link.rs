//! Links between machines on loopback (docs/link.md, "Testing"). Each machine is a link with its
//! own state directory and ports; some run in a child process (this test binary again), so they can
//! be killed without saying goodbye, or stopped.

use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use jackstay_net::peers::{self, Peer};
use jackstay_net::{
    Address, Config, Event, Handle, Identity, PairFailure, PeerState, PeerStatus, PublicKey,
    Refusal, Status, Timings, start,
};

const CHILD: &str = "JACKSTAY_NET_CHILD";

#[track_caller]
fn ok<T, E: std::fmt::Display>(r: Result<T, E>) -> T {
    r.unwrap_or_else(|e| panic!("{e}"))
}

/// A port with the next one also free, for a machine's main and pairing endpoints: from a range
/// of this process's own, so parallel tests and test binaries do not hand out the same port.
fn ports() -> u16 {
    const SPAN: u32 = 300;
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let base = 15_000 + (std::process::id() % 150) * SPAN;
    loop {
        // Never reused within the process: a port handed out may not be bound yet.
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

struct Machine {
    dir: PathBuf,
    port: u16,
    key: PublicKey,
}

impl Machine {
    fn new(name: &str) -> Machine {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("link-{}-{n}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        ok(std::fs::create_dir_all(&dir));
        let key = ok(Identity::load_or_create(&dir.join("identity"))).public();
        Machine { dir, port: ports(), key }
    }

    fn address(&self) -> Address {
        address(self.port)
    }

    fn config(&self, timings: Timings) -> Config {
        Config { state_dir: self.dir.clone(), port: self.port, name: "test".to_owned(), timings }
    }

    /// Records `other` in this machine's peers file, as pairing would.
    fn knows(&self, other: &Machine) {
        self.knows_at(other, other.address());
    }

    fn knows_at(&self, other: &Machine, address: Address) {
        let mut all = ok(peers::load(&self.dir.join("peers")));
        all.retain(|p| p.key != other.key);
        all.push(Peer { key: other.key, address, name: "peer".to_owned() });
        ok(peers::save(&self.dir.join("peers"), &all));
    }

    fn start(&self) -> Link {
        self.start_with(Timings::default())
    }

    fn start_with(&self, timings: Timings) -> Link {
        let (tx, events) = channel();
        let handle = ok(start(self.config(timings), move |e| {
            let _ = tx.send(e);
        }));
        Link { handle, events, last: None }
    }

    /// This machine in a child process, which can be killed or stopped.
    fn spawn(&self) -> Kid {
        self.spawn_as("idle")
    }

    /// This machine in a child process, its sessions with every peer active.
    fn spawn_active(&self) -> Kid {
        self.spawn_as("active")
    }

    fn spawn_as(&self, mode: &str) -> Kid {
        let me = ok(std::env::current_exe());
        let mut child = ok(Command::new(me)
            .args(["--exact", "child", "--nocapture", "--test-threads", "1"])
            .env(CHILD, format!("{} {} {mode}", self.dir.display(), self.port))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn());
        // The child's link binds within milliseconds; give it the time to.
        std::thread::sleep(Duration::from_millis(200));
        assert!(ok(child.try_wait()).is_none(), "child exited");
        Kid(child)
    }
}

/// A child process, killed when dropped, so a failing test leaves none behind. The child also
/// exits when its standard input closes, so it goes with this process however that ends.
struct Kid(Child);

impl Kid {
    fn kill(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }

    fn pid(&self) -> rustix::process::Pid {
        rustix::process::Pid::from_child(&self.0)
    }

    fn signal(&self, signal: rustix::process::Signal) {
        ok(rustix::process::kill_process(self.pid(), signal));
    }

    /// Processor time the child has used, from /proc: user plus system, in clock ticks.
    fn cpu_ticks(&self) -> u64 {
        let stat = ok(std::fs::read_to_string(format!("/proc/{}/stat", self.0.id())));
        // Fields after the command name, which is in parentheses and may hold spaces.
        let after = stat.rsplit_once(')').map_or("", |(_, rest)| rest);
        let fields: Vec<&str> = after.split_whitespace().collect();
        // utime and stime are fields 14 and 15 of the whole line: 12 and 13 here.
        fields[11].parse::<u64>().unwrap_or(0) + fields[12].parse::<u64>().unwrap_or(0)
    }
}

impl Drop for Kid {
    fn drop(&mut self) {
        self.kill();
    }
}

fn address(port: u16) -> Address {
    Address::parse(&format!("127.0.0.1:{port}")).unwrap_or_else(|| panic!("address"))
}

struct Link {
    handle: Handle,
    events: Receiver<Event>,
    last: Option<Status>,
}

impl Link {
    /// Waits up to `within` for an event `pick` returns something for.
    fn wait<T>(
        &mut self,
        within: Duration,
        what: &str,
        mut pick: impl FnMut(&Event) -> Option<T>,
    ) -> T {
        let until = Instant::now() + within;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            match self.events.recv_timeout(left) {
                Ok(e) => {
                    if let Event::Status(s) = &e {
                        self.last = Some(s.clone());
                    }
                    if let Some(t) = pick(&e) {
                        return t;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    panic!("no {what} within {within:?}; last status {:?}", self.last)
                }
                Err(RecvTimeoutError::Disconnected) => panic!("link ended waiting for {what}"),
            }
        }
    }

    /// Waits for the peer with `key` to be in `state`, and returns its status.
    fn peer_in(&mut self, key: PublicKey, state: PeerState, within: Duration) -> PeerStatus {
        self.wait(within, &format!("{state:?}"), |e| match e {
            Event::Status(s) => s.peers.iter().find(|p| p.key == key && p.state == state).cloned(),
            _ => None,
        })
    }

    /// How long, within `within`, the peer with `key` stays in `state` without leaving it.
    fn stays(&mut self, key: PublicKey, state: PeerState, within: Duration) -> bool {
        let until = Instant::now() + within;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            match self.events.recv_timeout(left) {
                Ok(Event::Status(s)) => {
                    if s.peers.iter().any(|p| p.key == key && p.state != state) {
                        self.last = Some(s);
                        return false;
                    }
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => return true,
                Err(RecvTimeoutError::Disconnected) => return false,
            }
        }
    }
}

const SOON: Duration = Duration::from_secs(5);

/// The child: a machine's link, until it is killed.
#[test]
fn child() {
    let Ok(spec) = std::env::var(CHILD) else { return };
    let mut fields = spec.rsplitn(3, ' ');
    let (Some(mode), Some(port), Some(dir)) = (fields.next(), fields.next(), fields.next()) else {
        panic!("{spec}")
    };
    let config = Config {
        state_dir: PathBuf::from(dir),
        port: port.parse().unwrap_or_else(|e| panic!("{e}")),
        name: "child".to_owned(),
        timings: Timings::default(),
    };
    let peers = ok(peers::load(&config.state_dir.join("peers")));
    let handle = ok(start(config, |_| {}));
    // Gone with the parent, however it ends: its end of our standard input closes.
    std::thread::spawn(|| {
        let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
        std::process::exit(0);
    });
    if mode == "active" {
        for p in peers {
            handle.set_active(p.key, true);
        }
    }
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

#[test]
fn paired_machines_connect_and_measure_round_trips() {
    let (a, b) = (Machine::new("a"), Machine::new("b"));
    a.knows(&b);
    b.knows(&a);
    let (mut la, mut lb) = (a.start(), b.start());
    la.peer_in(b.key, PeerState::Connected, SOON);
    lb.peer_in(a.key, PeerState::Connected, SOON);
    let measured = la.wait(SOON, "a round trip", |e| match e {
        Event::Status(s) => s.peers.iter().find(|p| p.key == b.key).and_then(|p| p.rtt),
        _ => None,
    });
    assert!(measured < Duration::from_millis(100), "{measured:?}");
    let s = la.last.clone().unwrap_or_else(|| panic!("no status"));
    assert_eq!(s.listening, Ok(a.port));
    assert_eq!(s.fingerprint, a.key.fingerprint());
}

#[test]
fn both_machines_dialing_at_once_settle_on_one_session_that_stays() {
    // Neither waits before dialing, so both sessions come up together, round after round; each
    // machine must keep the same one, the session the smaller key dialed.
    let at_once = || Timings { defer_dial: Duration::ZERO, ..Timings::default() };
    for _ in 0..4 {
        let (a, b) = (Machine::new("a"), Machine::new("b"));
        a.knows(&b);
        b.knows(&a);
        let (mut la, mut lb) = (a.start_with(at_once()), b.start_with(at_once()));
        la.peer_in(b.key, PeerState::Connected, SOON);
        lb.peer_in(a.key, PeerState::Connected, SOON);
        assert!(la.stays(b.key, PeerState::Connected, Duration::from_secs(2)), "{:?}", la.last);
        assert!(lb.stays(a.key, PeerState::Connected, Duration::from_millis(200)), "{:?}", lb.last);
    }
}

#[test]
fn a_machine_that_cannot_dial_its_peer_is_dialed_by_it() {
    let (a, b) = (Machine::new("a"), Machine::new("b"));
    // a has a wrong address for b (nothing listens there); b has a's right one.
    let nowhere = ports();
    a.knows_at(&b, address(nowhere));
    b.knows(&a);
    let (mut la, mut lb) = (a.start(), b.start());
    la.peer_in(b.key, PeerState::Connected, SOON);
    lb.peer_in(a.key, PeerState::Connected, SOON);
}

#[test]
fn an_unpaired_machine_is_refused_in_the_handshake() {
    let (a, stranger) = (Machine::new("a"), Machine::new("stranger"));
    // The stranger knows a; a does not know the stranger.
    stranger.knows(&a);
    let _la = a.start();
    let mut ls = stranger.start();
    let refused = ls.peer_in(a.key, PeerState::Refused(Refusal::UnknownKey), SOON);
    assert_eq!(refused.rtt, None);
}

#[test]
fn a_machine_showing_another_key_is_refused() {
    let (a, b, impostor) = (Machine::new("a"), Machine::new("b"), Machine::new("impostor"));
    // a expects b at the impostor's port; the impostor knows a, so only a's check can refuse it.
    a.knows_at(&b, impostor.address());
    impostor.knows(&a);
    let mut la = a.start();
    let _li = impostor.start();
    // The impostor proves its own key, which a has not paired: a refuses it.
    la.peer_in(b.key, PeerState::Refused(Refusal::UnknownKey), SOON);
    assert!(la.stays(b.key, PeerState::Refused(Refusal::UnknownKey), Duration::from_secs(2)));
}

#[test]
fn an_unpaired_peer_cannot_come_back() {
    let (a, b) = (Machine::new("a"), Machine::new("b"));
    a.knows(&b);
    b.knows(&a);
    let (mut la, mut lb) = (a.start(), b.start());
    la.peer_in(b.key, PeerState::Connected, SOON);
    lb.handle.unpair(a.key);
    lb.wait(SOON, "unpaired", |e| match e {
        Event::Unpaired { fingerprint, saved } => {
            assert_eq!(saved, &Ok(()));
            (fingerprint == &a.key.fingerprint()).then_some(())
        }
        _ => None,
    });
    assert_eq!(ok(peers::load(&b.dir.join("peers"))), Vec::new());
    // a is told, and every later attempt is refused: no ticket lets it resume.
    la.peer_in(b.key, PeerState::Refused(Refusal::UnknownKey), SOON);
    assert!(la.stays(b.key, PeerState::Refused(Refusal::UnknownKey), Duration::from_secs(3)));
}

#[test]
fn a_restarted_peer_is_linked_again_at_once() {
    // Killed without a goodbye and started again. Its old session is reset at a's next packet,
    // or replaced by its new run's dial, whichever comes first: well within the 10 s idle timeout.
    let (a, b) = (Machine::new("a"), Machine::new("b"));
    a.knows(&b);
    b.knows(&a);
    let mut la = a.start();
    let mut child = b.spawn();
    let first = la.peer_in(b.key, PeerState::Connected, SOON).connects;
    child.kill();
    let killed = Instant::now();
    let mut child = b.spawn();
    la.wait(SOON, "a new session", |e| match e {
        Event::Status(s) => s
            .peers
            .iter()
            .any(|p| p.key == b.key && p.state == PeerState::Connected && p.connects > first)
            .then_some(()),
        _ => None,
    });
    assert!(killed.elapsed() < Duration::from_secs(3), "{:?}", killed.elapsed());
    child.kill();
}

#[test]
fn a_restarted_peer_that_cannot_dial_back_resets_the_old_session() {
    // Here only the reset brings the link back early: b has a wrong address for a, so a must
    // notice the restart itself. Without b's reset a would wait out the 10 s idle timeout.
    let (a, b) = (Machine::new("a"), Machine::new("b"));
    a.knows(&b);
    b.knows_at(&a, address(ports()));
    let mut la = a.start();
    let mut child = b.spawn();
    la.peer_in(b.key, PeerState::Connected, SOON);
    child.kill();
    let killed = Instant::now();
    let mut child = b.spawn();
    la.wait(SOON, "the session to go", |e| match e {
        Event::Status(s) => {
            s.peers.iter().any(|p| p.key == b.key && p.state == PeerState::Dialing).then_some(())
        }
        _ => None,
    });
    la.peer_in(b.key, PeerState::Connected, SOON);
    assert!(killed.elapsed() < Duration::from_secs(4), "{:?}", killed.elapsed());
    child.kill();
}

#[test]
fn a_killed_peer_that_stays_away_is_dialed_without_end() {
    let (a, b) = (Machine::new("a"), Machine::new("b"));
    a.knows(&b);
    b.knows(&a);
    let mut la = a.start();
    let mut child = b.spawn();
    la.peer_in(b.key, PeerState::Connected, SOON);
    child.kill();
    // Silence: suspect after three of the peer's 1 s intervals, dead at the 10 s idle timeout.
    la.peer_in(b.key, PeerState::Suspect, Duration::from_secs(5));
    la.peer_in(b.key, PeerState::Dialing, Duration::from_secs(10));
    // It comes back long after; the next dial, at most 2 s away, finds it.
    let mut child = b.spawn();
    la.peer_in(b.key, PeerState::Connected, Duration::from_secs(5));
    child.kill();
}

#[test]
fn a_stopped_peer_is_suspected_after_three_of_its_intervals() {
    let (a, b) = (Machine::new("a"), Machine::new("b"));
    a.knows(&b);
    b.knows(&a);
    let mut la = a.start();
    let mut child = b.spawn();
    la.peer_in(b.key, PeerState::Connected, SOON);
    // The child is idle: it pings every second, so a suspects it after 3 s of silence.
    child.signal(rustix::process::Signal::STOP);
    let stopped = Instant::now();
    la.peer_in(b.key, PeerState::Suspect, Duration::from_secs(5));
    let took = stopped.elapsed();
    assert!(took >= Duration::from_millis(1900) && took < Duration::from_millis(4500), "{took:?}");
    child.signal(rustix::process::Signal::CONT);
    la.peer_in(b.key, PeerState::Connected, SOON);
    child.kill();
}

#[test]
fn an_active_peer_stopped_is_suspected_within_300_ms() {
    let (a, b) = (Machine::new("a"), Machine::new("b"));
    a.knows(&b);
    b.knows(&a);
    let mut la = a.start();
    let mut child = b.spawn_active();
    la.peer_in(b.key, PeerState::Connected, SOON);
    // The first session may be replaced: when the larger key's deferred dial comes up first, the
    // smaller key's dial replaces it as soon as its backoff (at most 2 s) and the dial (at most
    // 1.5 s) allow. A child stopped then has sent no ping on the new session, which then expects
    // the idle interval. So wait until no replacement can come, and then for a round trip on the
    // session kept: the child is active, so a expects to hear within 300 ms.
    let t = Timings::default();
    std::thread::sleep(t.backoff_max + t.dial_deadline + Duration::from_millis(500));
    la.wait(SOON, "a round trip", |e| match e {
        Event::Status(s) => s.peers.iter().find(|p| p.key == b.key && p.rtt.is_some()).map(|_| ()),
        _ => None,
    });
    child.signal(rustix::process::Signal::STOP);
    let stopped = Instant::now();
    la.peer_in(b.key, PeerState::Suspect, Duration::from_secs(2));
    let took = stopped.elapsed();
    assert!(took < Duration::from_millis(600), "{took:?}");
    child.signal(rustix::process::Signal::CONT);
    la.peer_in(b.key, PeerState::Connected, SOON);
    child.kill();
}

fn pair_both(la: &mut Link, a: &Machine, lb: &mut Link, b: &Machine) -> (String, String) {
    la.handle.pair(b.address());
    lb.handle.pair(a.address());
    let code = |l: &mut Link| {
        l.wait(SOON, "a code", |e| match e {
            Event::PairCode { code, fingerprint, .. } => Some((code.clone(), fingerprint.clone())),
            Event::PairFailed(f) => panic!("pairing failed: {f}"),
            _ => None,
        })
    };
    let (ca, fa) = code(la);
    let (cb, fb) = code(lb);
    assert_eq!(fa, b.key.fingerprint());
    assert_eq!(fb, a.key.fingerprint());
    (ca, cb)
}

#[test]
fn pairing_shows_one_code_and_records_both_machines() {
    let (a, b) = (Machine::new("a"), Machine::new("b"));
    let (mut la, mut lb) = (a.start(), b.start());
    let (ca, cb) = pair_both(&mut la, &a, &mut lb, &b);
    assert_eq!(ca, cb);
    assert_eq!(ca.len(), 6);
    la.handle.confirm(true);
    lb.handle.confirm(true);
    for (l, other) in [(&mut la, &b), (&mut lb, &a)] {
        l.wait(SOON, "paired", |e| match e {
            Event::Paired { fingerprint, .. } => {
                (fingerprint == &other.key.fingerprint()).then_some(())
            }
            Event::PairFailed(f) => panic!("pairing failed: {f}"),
            _ => None,
        });
    }
    let on_a = ok(peers::load(&a.dir.join("peers")));
    assert_eq!(on_a.iter().map(|p| p.key).collect::<Vec<_>>(), [b.key]);
    assert_eq!(on_a[0].address, b.address());
    assert_eq!(on_a[0].name, "test");
    // And then they link as paired machines do.
    la.peer_in(b.key, PeerState::Connected, SOON);
    lb.peer_in(a.key, PeerState::Connected, SOON);
}

#[test]
fn pairing_twice_gives_different_codes() {
    let mut codes = Vec::new();
    for _ in 0..2 {
        let (a, b) = (Machine::new("a"), Machine::new("b"));
        let (mut la, mut lb) = (a.start(), b.start());
        let (ca, _) = pair_both(&mut la, &a, &mut lb, &b);
        codes.push(ca);
        la.handle.cancel_pairing();
        lb.handle.cancel_pairing();
    }
    // One chance in a million of a false failure.
    assert_ne!(codes[0], codes[1]);
}

#[test]
fn a_no_on_either_machine_records_nothing() {
    let (a, b) = (Machine::new("a"), Machine::new("b"));
    let (mut la, mut lb) = (a.start(), b.start());
    pair_both(&mut la, &a, &mut lb, &b);
    la.handle.confirm(true);
    lb.handle.confirm(false);
    let failed = |l: &mut Link| {
        l.wait(SOON, "failure", |e| match e {
            Event::PairFailed(f) => Some(f.clone()),
            Event::Paired { .. } => panic!("paired"),
            _ => None,
        })
    };
    assert_eq!(failed(&mut lb), PairFailure::Declined);
    assert_eq!(failed(&mut la), PairFailure::PeerEnded);
    assert_eq!(ok(peers::load(&a.dir.join("peers"))), Vec::new());
    assert_eq!(ok(peers::load(&b.dir.join("peers"))), Vec::new());
}

#[test]
fn a_second_machine_trying_to_pair_stops_pairing() {
    let (a, b, c) = (Machine::new("a"), Machine::new("b"), Machine::new("c"));
    let (mut la, lb, lc) = (a.start(), b.start(), c.start());
    // b and c both pair with a; a pairs with b. a sees two handshakes.
    la.handle.pair(b.address());
    lb.handle.pair(a.address());
    lc.handle.pair(a.address());
    let failure = la.wait(SOON, "failure", |e| match e {
        Event::PairFailed(f) => Some(f.clone()),
        _ => None,
    });
    assert_eq!(failure, PairFailure::SecondAttempt);
    assert_eq!(ok(peers::load(&a.dir.join("peers"))), Vec::new());
}

#[test]
fn a_second_handshake_with_the_same_key_stops_pairing() {
    // A copy of b's disk (same key) pairs with a as well: two handshakes, one key. Only the count
    // of handshakes can catch it.
    let (a, b, copy) = (Machine::new("a"), Machine::new("b"), Machine::new("copy"));
    ok(std::fs::copy(b.dir.join("identity"), copy.dir.join("identity")));
    let (mut la, lb, lcopy) = (a.start(), b.start(), copy.start());
    la.handle.pair(b.address());
    lb.handle.pair(a.address());
    lcopy.handle.pair(a.address());
    let failure = la.wait(SOON, "failure", |e| match e {
        Event::PairFailed(f) => Some(f.clone()),
        Event::Paired { .. } => panic!("paired"),
        _ => None,
    });
    assert_eq!(failure, PairFailure::SecondAttempt);
}

#[test]
fn pairing_times_out() {
    let (a, b) = (Machine::new("a"), Machine::new("b"));
    let timings = Timings { pairing_window: Duration::from_secs(2), ..Timings::default() };
    let mut la = a.start_with(timings);
    la.handle.pair(b.address());
    let failure = la.wait(SOON, "failure", |e| match e {
        Event::PairFailed(f) => Some(f.clone()),
        _ => None,
    });
    assert_eq!(failure, PairFailure::Timeout);
}

#[test]
fn a_second_pairing_while_one_runs_is_refused() {
    let (a, b) = (Machine::new("a"), Machine::new("b"));
    let mut la = a.start();
    la.handle.pair(b.address());
    la.handle.pair(b.address());
    let failure = la.wait(SOON, "failure", |e| match e {
        Event::PairFailed(f) => Some(f.clone()),
        _ => None,
    });
    assert_eq!(failure, PairFailure::Busy);
}

#[test]
fn a_stranger_is_refused_on_the_main_port_while_pairing_runs() {
    let (a, b, stranger) = (Machine::new("a"), Machine::new("b"), Machine::new("stranger"));
    stranger.knows(&a);
    let mut la = a.start();
    la.handle.pair(b.address());
    la.wait(SOON, "pairing", |e| match e {
        Event::Status(s) => s.pairing.then_some(()),
        _ => None,
    });
    let mut ls = stranger.start();
    ls.peer_in(a.key, PeerState::Refused(Refusal::UnknownKey), SOON);
}

/// A verifier for test peers that accepts any raw Ed25519 key, as either end, and checks nothing:
/// the stand-ins below play machines that are not ours.
#[derive(Debug)]
struct AnyKey;

impl rustls::client::danger::ServerCertVerifier for AnyKey {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![rustls::SignatureScheme::ED25519]
    }
    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

impl rustls::server::danger::ClientCertVerifier for AnyKey {
    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &[]
    }
    fn verify_client_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        Ok(rustls::server::danger::ClientCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![rustls::SignatureScheme::ED25519]
    }
    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

/// A peer that speaks another version: a bare QUIC server whose only ALPN is not ours.
fn other_version(port: u16) -> (tokio::runtime::Runtime, quinn::Endpoint) {
    use std::sync::Arc;
    let pkcs8 =
        ok(ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
            .map_err(|_| "no key"));
    let der = rustls::pki_types::PrivatePkcs8KeyDer::from(pkcs8.as_ref().to_vec());
    let signer = ok(rustls::crypto::ring::sign::any_eddsa_type(&der));
    let spki = signer.public_key().unwrap_or_else(|| panic!("no spki")).as_ref().to_vec();
    let certified = Arc::new(rustls::sign::CertifiedKey::new(
        vec![rustls::pki_types::CertificateDer::from(spki)],
        signer,
    ));
    let mut tls = ok(rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13]))
    .with_client_cert_verifier(Arc::new(AnyKey))
    .with_cert_resolver(Arc::new(rustls::server::AlwaysResolvesServerRawPublicKeys::new(
        certified,
    )));
    tls.alpn_protocols = vec![b"jackstay/0".to_vec()];
    let quic = ok(quinn::crypto::rustls::QuicServerConfig::try_from(tls));
    let rt = ok(tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build());
    let endpoint = ok(rt.block_on(async {
        quinn::Endpoint::server(
            quinn::ServerConfig::with_crypto(Arc::new(quic)),
            std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        )
    }));
    let accepting = endpoint.clone();
    rt.spawn(async move {
        while let Some(incoming) = accepting.accept().await {
            tokio::spawn(async move {
                let _ = incoming.await;
            });
        }
    });
    (rt, endpoint)
}

#[test]
fn a_peer_of_another_version_is_named_as_such() {
    let (a, b) = (Machine::new("a"), Machine::new("b"));
    a.knows(&b);
    let (_rt, _old) = other_version(b.port);
    let mut la = a.start();
    la.peer_in(b.key, PeerState::Refused(Refusal::Version), SOON);
}

/// A client holding `dir`'s identity that keeps its session tickets, as an unpaired machine
/// trying to come back would.
fn ticket_keeper(dir: &Path) -> quinn::ClientConfig {
    use std::sync::Arc;
    let pkcs8 = ok(std::fs::read(dir.join("identity")));
    let der = rustls::pki_types::PrivatePkcs8KeyDer::from(pkcs8);
    let signer = ok(rustls::crypto::ring::sign::any_eddsa_type(&der));
    let spki = signer.public_key().unwrap_or_else(|| panic!("no spki")).as_ref().to_vec();
    let certified = Arc::new(rustls::sign::CertifiedKey::new(
        vec![rustls::pki_types::CertificateDer::from(spki)],
        signer,
    ));
    let mut tls = ok(rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13]))
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(AnyKey))
    .with_client_cert_resolver(Arc::new(rustls::client::AlwaysResolvesClientRawPublicKeys::new(
        certified,
    )));
    tls.alpn_protocols = vec![jackstay_net::ALPN.to_vec()];
    quinn::ClientConfig::new(Arc::new(ok(quinn::crypto::rustls::QuicClientConfig::try_from(tls))))
}

#[test]
fn a_session_ticket_does_not_bring_an_unpaired_machine_back() {
    let (a, b) = (Machine::new("a"), Machine::new("b"));
    b.knows(&a);
    let mut lb = b.start();
    lb.wait(SOON, "listening", |e| match e {
        Event::Status(s) => s.listening.is_ok().then_some(()),
        _ => None,
    });
    // a's key, from a client that keeps whatever tickets it is given.
    let client = ticket_keeper(&a.dir);
    let rt = ok(tokio::runtime::Builder::new_current_thread().enable_all().build());
    let server = std::net::SocketAddr::from(([127, 0, 0, 1], b.port));
    let dial = |client: &quinn::ClientConfig| {
        rt.block_on(async {
            let endpoint = ok(quinn::Endpoint::client(
                "127.0.0.1:0".parse().unwrap_or_else(|e| panic!("{e}")),
            ));
            let c = ok(endpoint.connect_with(client.clone(), server, "jackstay"))
                .await
                .map_err(|e| e.to_string())?;
            // A client finishes its side of the handshake before the server's refusal arrives;
            // and any ticket arrives after it. Wait for both.
            tokio::time::sleep(Duration::from_millis(200)).await;
            if let Some(e) = c.close_reason() {
                return Err(e.to_string());
            }
            c.close(0u32.into(), b"");
            Ok(())
        })
    };
    assert_eq!(dial(&client), Ok(()), "paired: a full handshake succeeds");
    lb.handle.unpair(a.key);
    lb.wait(SOON, "unpaired", |e| matches!(e, Event::Unpaired { .. }).then_some(()));
    let again = dial(&client);
    assert!(again.as_ref().is_err_and(|e| e.contains("48")), "unpaired: {again:?}");
}

#[test]
fn an_active_side_notices_a_silent_idle_peer_within_300_ms() {
    // The primary in M4 is the active side; its peer may still be idle (1 s pings). Three of the
    // primary's own 100 ms pings unanswered are enough.
    let (a, b) = (Machine::new("a"), Machine::new("b"));
    a.knows(&b);
    b.knows(&a);
    let mut la = a.start();
    let child = b.spawn();
    la.peer_in(b.key, PeerState::Connected, SOON);
    la.handle.set_active(b.key, true);
    std::thread::sleep(Duration::from_millis(500));
    child.signal(rustix::process::Signal::STOP);
    let stopped = Instant::now();
    la.peer_in(b.key, PeerState::Suspect, Duration::from_secs(2));
    let took = stopped.elapsed();
    assert!(took < Duration::from_millis(600), "{took:?}");
    child.signal(rustix::process::Signal::CONT);
    la.peer_in(b.key, PeerState::Connected, SOON);
}

#[test]
fn a_busy_port_is_retried_without_spinning() {
    // Something else holds the port: the link reports it and waits for its retry, using no
    // processor time meanwhile, though a peer's dial is due.
    let (a, b) = (Machine::new("a"), Machine::new("b"));
    a.knows(&b);
    let _holder = ok(UdpSocket::bind(("0.0.0.0", a.port)));
    let child = a.spawn();
    std::thread::sleep(Duration::from_millis(300));
    let before = child.cpu_ticks();
    std::thread::sleep(Duration::from_secs(2));
    let used = child.cpu_ticks() - before;
    // Clock ticks are 10 ms: a spinning thread would use about 200 in 2 s.
    assert!(used < 20, "{used} ticks in 2 s");
    let mut la = a.start();
    let status = la.wait(SOON, "status", |e| match e {
        Event::Status(s) => Some(s.clone()),
        _ => None,
    });
    assert!(status.listening.is_err(), "{status:?}");
}

#[test]
fn pairing_an_existing_peer_again_keeps_its_session() {
    let (a, b) = (Machine::new("a"), Machine::new("b"));
    a.knows(&b);
    b.knows(&a);
    let (mut la, mut lb) = (a.start(), b.start());
    la.peer_in(b.key, PeerState::Connected, SOON);
    lb.peer_in(a.key, PeerState::Connected, SOON);
    pair_both(&mut la, &a, &mut lb, &b);
    la.handle.confirm(true);
    lb.handle.confirm(true);
    // The session it had stays, through pairing and after: never anything but connected.
    let still = |e: &Event| {
        if let Event::Status(s) = e
            && let Some(p) = s.peers.iter().find(|p| p.key == b.key)
        {
            assert_eq!((p.state, p.connects), (PeerState::Connected, 1), "{s:?}");
        }
    };
    la.wait(SOON, "paired", |e| {
        still(e);
        matches!(e, Event::Paired { .. }).then_some(())
    });
    // The record is replaced, not added to.
    assert_eq!(ok(peers::load(&a.dir.join("peers"))).len(), 1);
    let until = Instant::now() + Duration::from_secs(1);
    while let Ok(e) = la.events.recv_timeout(until.saturating_duration_since(Instant::now())) {
        still(&e);
    }
}

#[test]
fn a_peers_file_naming_this_machine_is_refused() {
    let a = Machine::new("a");
    a.knows(&a);
    match start(a.config(Timings::default()), |_| {}) {
        Err(jackstay_net::StartError::Peers(jackstay_net::PeersError::OwnKey)) => {}
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_dial_to_a_black_hole_gives_up_at_its_deadline() {
    // The peer's address is a socket that reads and drops: each dial is one connection ID, sent
    // again and again until the attempt's 1.5 s deadline; the next attempt has a new one.
    let (a, b) = (Machine::new("a"), Machine::new("b"));
    let hole = ok(UdpSocket::bind(("127.0.0.1", b.port)));
    ok(hole.set_read_timeout(Some(Duration::from_millis(100))));
    a.knows(&b);
    let _la = a.start();
    let mut attempts: Vec<(Vec<u8>, Instant)> = Vec::new();
    let until = Instant::now() + Duration::from_secs(6);
    let mut buf = [0u8; 2048];
    while Instant::now() < until && attempts.len() < 3 {
        let Ok(n) = hole.recv(&mut buf) else { continue };
        // A QUIC long header: flags, version (4), then the destination ID's length and bytes.
        let packet = &buf[..n];
        if packet.len() < 6 || packet[0] & 0x80 == 0 {
            continue;
        }
        let len = usize::from(packet[5]);
        let Some(id) = packet.get(6..6 + len) else { continue };
        if !attempts.iter().any(|(seen, _)| seen == id) {
            attempts.push((id.to_vec(), Instant::now()));
        }
    }
    assert!(attempts.len() >= 2, "{} attempts", attempts.len());
    let gap = attempts[1].1 - attempts[0].1;
    // 1.5 s for the attempt, then at most the larger key's 500 ms wait.
    assert!(gap >= Duration::from_millis(1300) && gap < Duration::from_millis(2600), "{gap:?}");
}

/// A machine that pairs by the protocol until `misstep`, then breaks it. It dials `target`'s
/// pairing port from `from`, with a key of its own, smaller than `than` if `smaller`.
fn rogue_pairing(
    target: u16,
    from: std::net::IpAddr,
    misstep: &'static str,
    smaller: bool,
    than: PublicKey,
) -> std::thread::JoinHandle<()> {
    use jackstay_proto::link::{self as wire, Message};
    use std::sync::Arc;
    std::thread::spawn(move || {
        // A key on the chosen side of `than`: half of all keys are.
        let (signer, spki) = loop {
            let pkcs8 = ok(ring::signature::Ed25519KeyPair::generate_pkcs8(
                &ring::rand::SystemRandom::new(),
            )
            .map_err(|_| "no key"));
            let der = rustls::pki_types::PrivatePkcs8KeyDer::from(pkcs8.as_ref().to_vec());
            let signer = ok(rustls::crypto::ring::sign::any_eddsa_type(&der));
            let spki = signer.public_key().unwrap_or_else(|| panic!("no spki")).as_ref().to_vec();
            if (spki.as_slice() < than.as_bytes().as_slice()) == smaller {
                break (signer, spki);
            }
        };
        let certified = Arc::new(rustls::sign::CertifiedKey::new(
            vec![rustls::pki_types::CertificateDer::from(spki)],
            signer,
        ));
        let mut tls = ok(rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13]))
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyKey))
        .with_client_cert_resolver(Arc::new(
            rustls::client::AlwaysResolvesClientRawPublicKeys::new(certified),
        ));
        tls.alpn_protocols = vec![jackstay_net::PAIR_ALPN.to_vec()];
        let client = quinn::ClientConfig::new(Arc::new(ok(
            quinn::crypto::rustls::QuicClientConfig::try_from(tls),
        )));
        let rt = ok(tokio::runtime::Builder::new_current_thread().enable_all().build());
        rt.block_on(async move {
            let endpoint = ok(quinn::Endpoint::client(std::net::SocketAddr::new(from, 0)));
            let server = std::net::SocketAddr::from(([127, 0, 0, 1], target + 1));
            let Ok(Ok(conn)) = tokio::time::timeout(
                Duration::from_secs(3),
                ok(endpoint.connect_with(client, server, "jackstay")),
            )
            .await
            else {
                return;
            };
            let Ok((mut send, mut recv)) = conn.open_bi().await else { return };
            let write = |m: Message| {
                let mut b = Vec::new();
                ok(wire::encode(&m, &mut b));
                b
            };
            let hello = Message::Hello { version: wire::VERSION, run: 9, name: "rogue".into() };
            if send.write_all(&write(hello)).await.is_err() {
                return;
            }
            let mut buf = Vec::new();
            let mut chunk = [0u8; 512];
            let mut read = async || -> Option<Message> {
                loop {
                    if let Ok(Some((m, used))) = wire::decode(&buf) {
                        buf.drain(..used);
                        return Some(m);
                    }
                    let n = recv.read(&mut chunk).await.ok()??;
                    buf.extend_from_slice(&chunk[..n]);
                }
            };
            let _hello = read().await;
            let nonce = [7u8; wire::NONCE_LEN];
            let digest =
                <[u8; 32]>::try_from(ring::digest::digest(&ring::digest::SHA256, &nonce).as_ref())
                    .unwrap_or_default();
            match misstep {
                "reveal-first" => {
                    let _ = send.write_all(&write(Message::Reveal { nonce })).await;
                }
                "wrong-reveal" => {
                    let _ = send.write_all(&write(Message::Commit { digest })).await;
                    while let Some(m) = read().await {
                        if let Message::Commit { .. } = m {
                            break;
                        }
                    }
                    let other = [8u8; wire::NONCE_LEN];
                    let _ = send.write_all(&write(Message::Reveal { nonce: other })).await;
                }
                _ => {}
            }
            // Hold the session until the other machine ends it.
            let _ = tokio::time::timeout(Duration::from_secs(5), conn.closed()).await;
        });
    })
}

fn pairing_fails_with(misstep: &'static str, from: [u8; 4], smaller: bool, expected: &PairFailure) {
    let (a, b) = (Machine::new("a"), Machine::new("b"));
    let mut la = a.start();
    // a pairs with "b" at 127.0.0.1; the rogue comes instead.
    la.handle.pair(b.address());
    la.wait(SOON, "pairing", |e| match e {
        Event::Status(s) => s.pairing.then_some(()),
        _ => None,
    });
    let rogue = rogue_pairing(a.port, std::net::IpAddr::from(from), misstep, smaller, a.key);
    let failure = la.wait(SOON, "failure", |e| match e {
        Event::PairFailed(f) => Some(f.clone()),
        Event::PairCode { .. } => panic!("a code was shown"),
        Event::Paired { .. } => panic!("paired"),
        _ => None,
    });
    assert_eq!(&failure, expected);
    let _ = rogue.join();
}

// Each protocol break with the rogue's key on either side of ours: which of the two machines
// commits first, and so which code path meets the break, depends on it.

#[test]
fn a_reveal_that_does_not_match_its_commitment_stops_pairing() {
    pairing_fails_with("wrong-reveal", [127, 0, 0, 1], true, &PairFailure::BadReveal);
    pairing_fails_with("wrong-reveal", [127, 0, 0, 1], false, &PairFailure::BadReveal);
}

#[test]
fn a_reveal_before_any_commitment_stops_pairing() {
    pairing_fails_with("reveal-first", [127, 0, 0, 1], true, &PairFailure::BadReveal);
    pairing_fails_with("reveal-first", [127, 0, 0, 1], false, &PairFailure::BadReveal);
}

#[test]
fn a_pairing_handshake_from_another_address_stops_pairing() {
    pairing_fails_with("none", [127, 0, 0, 2], true, &PairFailure::SecondAttempt);
}

#[test]
fn active_sessions_do_not_flood_status() {
    // 100 ms pings measure ten round trips a second; status still goes out about once a second.
    let (a, b) = (Machine::new("a"), Machine::new("b"));
    a.knows(&b);
    b.knows(&a);
    let (mut la, lb) = (a.start(), b.start());
    la.peer_in(b.key, PeerState::Connected, SOON);
    la.handle.set_active(b.key, true);
    lb.handle.set_active(a.key, true);
    std::thread::sleep(Duration::from_millis(300));
    while la.events.try_recv().is_ok() {}
    let until = Instant::now() + Duration::from_secs(2);
    let mut sent = 0;
    while let Ok(e) = la.events.recv_timeout(until.saturating_duration_since(Instant::now())) {
        sent += usize::from(matches!(e, Event::Status(_)));
    }
    assert!((1..=4).contains(&sent), "{sent} status events in 2 s");
}

#[test]
fn a_tokio_task_that_panics_is_reported_by_its_join_handle() {
    // Plan §5.8 asked whether a tokio task's panic is contained by its JoinHandle. It is; the link
    // does not rely on it, and turns any task's panic into the link thread's (engine.rs).
    let rt = ok(tokio::runtime::Builder::new_current_thread().build());
    let joined = rt.block_on(async { tokio::spawn(async { panic!("planted") }).await });
    assert!(joined.is_err_and(|e| e.is_panic()));
}
