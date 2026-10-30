//! Two daemons linked through a relay that can hurt the traffic between them (docs/link.md,
//! "Testing"): each daemon with its own Xvfb, home and ports, on loopback, paired through the
//! control socket as `jackstay pair` pairs. The relay owns the addresses each knows the other by,
//! and forwards every datagram, unless told to delay, drop, duplicate, reorder or black-hole it.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use jackstay_proto::control::{LinkStatus, PairReason, PeerState, Request, Response};
use jackstayd::client::Client;

use super::{Daemon, Home, Xvfb, ask, ok};

/// Two ports, the second for pairing, from a range of this process's own, never reused.
pub fn ports() -> u16 {
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

/// What the relay does to traffic now. Losses are chances from 0 to 1.
#[derive(Debug, Clone, Default)]
pub struct Faults {
    /// Nothing passes either way.
    pub blackhole: bool,
    /// Loss on the way from A to B, and from B to A.
    pub loss_to_b: f64,
    pub loss_to_a: f64,
    /// Every datagram held this long.
    pub delay: Duration,
    pub duplicate: bool,
    /// Each datagram held back until the next has gone.
    pub reorder: bool,
    /// Datagrams longer than this are dropped (a path whose MTU is smaller).
    pub largest: Option<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Toward {
    A,
    B,
}

pub type Delayed = (Instant, Arc<Mutex<UdpSocket>>, SocketAddr, Vec<u8>);

/// Forwards between A and B: A knows B at `for_a`, B knows A at `for_b` (and the next ports, for
/// pairing).
pub struct Relay {
    pub faults: Arc<Mutex<Faults>>,
    pub for_a: u16,
    pub for_b: u16,
    pub stop: Arc<AtomicBool>,
    /// What A's datagrams to B's main port leave from; swapped to rebind.
    pub to_b: Arc<Mutex<UdpSocket>>,
    /// What B's datagrams to A's main port leave from; swapped to rebind.
    pub to_a: Arc<Mutex<UdpSocket>>,
    pub a: u16,
    pub b: u16,
    pub delayed: mpsc::Sender<Delayed>,
}

pub fn local(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

pub fn bound(port: u16) -> Arc<Mutex<UdpSocket>> {
    let s = ok(UdpSocket::bind(local(port)));
    ok(s.set_read_timeout(Some(Duration::from_millis(20))));
    Arc::new(Mutex::new(s))
}

pub fn clone(s: &Arc<Mutex<UdpSocket>>) -> Arc<Mutex<UdpSocket>> {
    Arc::new(Mutex::new(ok(ok(s.lock()).try_clone())))
}

impl Relay {
    pub fn new(a: u16, b: u16) -> Relay {
        Relay::with(a, b, true)
    }

    /// With `pairing` false, the pairing ports are left to a stand-in in the middle.
    pub fn with(a: u16, b: u16, pairing: bool) -> Relay {
        let (for_a, for_b) = (ports(), ports());
        // Delayed datagrams go through one thread, in order: every delay is the same.
        let (delayed, queue) = mpsc::channel::<Delayed>();
        std::thread::spawn(move || {
            for (due, socket, to, bytes) in queue {
                std::thread::sleep(due.saturating_duration_since(Instant::now()));
                let _ = ok(socket.lock()).send_to(&bytes, to);
            }
        });
        let (rb, ra) = (bound(for_a), bound(for_b));
        let relay = Relay {
            faults: Arc::new(Mutex::new(Faults::default())),
            for_a,
            for_b,
            stop: Arc::new(AtomicBool::new(false)),
            to_b: clone(&ra),
            to_a: clone(&rb),
            a,
            b,
            delayed,
        };
        // From A to B: arriving where A knows B, leaving from where B knows A; and back.
        relay.lane(&rb, &relay.to_b, local(b), Toward::B);
        relay.lane(&ra, &relay.to_a, local(a), Toward::A);
        if pairing {
            let (rb1, ra1) = (bound(for_a + 1), bound(for_b + 1));
            relay.lane(&rb1, &ra1, local(b + 1), Toward::B);
            relay.lane(&ra1, &rb1, local(a + 1), Toward::A);
        }
        relay
    }

    /// A thread that forwards what arrives on `from` to `to`, sent from `out` as it is then.
    pub fn lane(
        &self,
        from: &Arc<Mutex<UdpSocket>>,
        out: &Arc<Mutex<UdpSocket>>,
        to: SocketAddr,
        toward: Toward,
    ) {
        let from = ok(ok(from.lock()).try_clone());
        let out = Arc::clone(out);
        let (faults, stop, delayed) =
            (Arc::clone(&self.faults), Arc::clone(&self.stop), self.delayed.clone());
        std::thread::spawn(move || {
            let mut seed: u64 = 0x9e37_79b9_7f4a_7c15 ^ u64::from(to.port());
            let mut chance = move || {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                f64::from(u32::try_from(seed >> 32).unwrap_or(0)) / f64::from(u32::MAX)
            };
            let mut held: Option<Vec<u8>> = None;
            let mut buf = vec![0u8; 65_536];
            while !stop.load(Ordering::Relaxed) {
                let Ok(n) = from.recv(&mut buf) else { continue };
                let f = ok(faults.lock()).clone();
                let loss = if toward == Toward::B { f.loss_to_b } else { f.loss_to_a };
                if f.blackhole || chance() < loss || f.largest.is_some_and(|l| n > l) {
                    continue;
                }
                let mut batch = vec![buf[..n].to_vec()];
                if f.duplicate {
                    batch.push(buf[..n].to_vec());
                }
                if f.reorder {
                    match held.take() {
                        None => held = batch.pop(),
                        Some(earlier) => batch.push(earlier),
                    }
                }
                for bytes in batch {
                    if f.delay.is_zero() {
                        let _ = ok(out.lock()).send_to(&bytes, to);
                    } else {
                        let _ =
                            delayed.send((Instant::now() + f.delay, Arc::clone(&out), to, bytes));
                    }
                }
            }
        });
    }

    pub fn set(&self, faults: Faults) {
        *ok(self.faults.lock()) = faults;
    }

    /// From now on `who`'s datagrams reach the other from a new port, as after a NAT rebinding;
    /// the other's answers to that port reach `who`.
    pub fn rebind(&self, who: Toward) {
        let fresh = bound(0);
        let socket = ok(ok(fresh.lock()).try_clone());
        if who == Toward::A {
            // A's datagrams to B.
            *ok(self.to_b.lock()) = socket;
            self.lane(&fresh, &self.to_a, local(self.a), Toward::A);
        } else {
            *ok(self.to_a.lock()) = socket;
            self.lane(&fresh, &self.to_b, local(self.b), Toward::B);
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// A machine: its X server, home, link port, and its daemon while it runs.
pub struct Machine {
    pub x: Xvfb,
    pub home: Home,
    pub port: u16,
    pub daemon: Option<Daemon>,
}

impl Machine {
    pub fn new() -> Machine {
        Machine::with(Xvfb::start(), "")
    }

    /// A machine on `x`, with `more` added to its configuration.
    pub fn with(x: Xvfb, more: &str) -> Machine {
        let (home, port) = (Home::new(), ports());
        ok(std::fs::write(home.config(), format!("[link]\nport = {port}\n{more}")));
        let daemon = Some(Daemon::start(&x, &home));
        Machine { x, home, port, daemon }
    }

    pub fn link(&self) -> LinkStatus {
        match ask(&self.home, Request::Peers) {
            Response::Peers(l) => l,
            other => panic!("{other:?}"),
        }
    }

    /// Waits up to `within` for its link to satisfy `pick`.
    #[track_caller]
    pub fn wait<T>(
        &self,
        within: Duration,
        what: &str,
        mut pick: impl FnMut(&LinkStatus) -> Option<T>,
    ) -> T {
        let until = Instant::now() + within;
        loop {
            let l = self.link();
            if let Some(t) = pick(&l) {
                return t;
            }
            assert!(Instant::now() < until, "no {what} within {within:?}: {l:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Waits for its one peer to be in `state`; returns how many sessions it has kept.
    #[track_caller]
    pub fn peer_in(&self, state: PeerState, within: Duration) -> u32 {
        self.wait(within, &format!("{state:?}"), |l| {
            l.peers.first().filter(|p| p.state == state).map(|p| p.connects)
        })
    }

    /// The one peer's state and sessions, now.
    pub fn peer(&self) -> (PeerState, u32) {
        let l = self.link();
        let p = l.peers.first().unwrap_or_else(|| panic!("no peer: {l:?}"));
        (p.state, p.connects)
    }

    pub fn stop(&mut self) {
        if let Some(d) = self.daemon.take() {
            let _ = d.stop();
        }
    }

    pub fn restart(&mut self) {
        self.stop();
        self.daemon = Some(Daemon::start(&self.x, &self.home));
    }

    pub fn pid(&self) -> i32 {
        let d = self.daemon.as_ref().unwrap_or_else(|| panic!("not running"));
        i32::try_from(d.child.id()).unwrap_or(0)
    }

    pub fn signal(&self, signal: &str) {
        ok(std::process::Command::new("kill").args([signal, &self.pid().to_string()]).status());
    }
}

/// How pairing ended for one machine: the code it showed, then paired or why not.
pub type Paired = (Option<String>, Result<(), PairReason>);

/// Pairs `m` with `host` as `jackstay pair` does, answering `yes` to the code, on a thread.
pub fn pair(m: &Machine, host: String, yes: bool) -> JoinHandle<Paired> {
    let socket = m.home.socket();
    std::thread::spawn(move || {
        let mut c = ok(Client::connect(&socket, Duration::from_secs(10)));
        ok(c.set_timeout(Duration::from_secs(150)));
        ok(c.send(&Request::Pair { host }));
        let mut code = None;
        loop {
            match ok(c.receive()) {
                Response::Ok => {}
                Response::PairCode { code: shown, .. } => {
                    code = Some(shown);
                    ok(c.send(&Request::PairAnswer { yes }));
                }
                Response::Paired { .. } => return (code, Ok(())),
                Response::PairFailed { reason, .. } => return (code, Err(reason)),
                other => panic!("{other:?}"),
            }
        }
    })
}

/// Two daemons and the relay between them.
pub struct Rig {
    pub a: Machine,
    pub b: Machine,
    pub relay: Relay,
}

impl Rig {
    pub fn new() -> Rig {
        Rig::of(Machine::new(), Machine::new())
    }

    pub fn of(a: Machine, b: Machine) -> Rig {
        let relay = Relay::new(a.port, b.port);
        Rig { a, b, relay }
    }

    /// Pairs both ways through the relay, both saying yes; returns both codes.
    pub fn pair(&self) -> (Paired, Paired) {
        let on_a = pair(&self.a, format!("127.0.0.1:{}", self.relay.for_a), true);
        let on_b = pair(&self.b, format!("127.0.0.1:{}", self.relay.for_b), true);
        (ok(on_a.join().map_err(|_| "pairing on a")), ok(on_b.join().map_err(|_| "pairing on b")))
    }

    /// Paired, and linked on both sides.
    pub fn linked() -> Rig {
        Rig::new().link()
    }

    /// These two paired, and linked on both sides.
    pub fn link(self) -> Rig {
        let rig = self;
        let (on_a, on_b) = rig.pair();
        assert_eq!((on_a.1, on_b.1), (Ok(()), Ok(())));
        rig.a.peer_in(PeerState::Connected, SOON);
        rig.b.peer_in(PeerState::Connected, SOON);
        rig
    }
}

pub const SOON: Duration = Duration::from_secs(5);
