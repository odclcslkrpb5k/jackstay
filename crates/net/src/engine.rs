//! The link thread's one task: it owns the endpoint, every session and pairing, decides which
//! session with a peer is kept, and when to dial (docs/link.md, "Who dials", "Liveness").

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use jackstay_proto::link::{self, Message};
use quinn::{Connection, Endpoint, Incoming};
use rustix::time::{ClockId, clock_gettime};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep_until, timeout};

use crate::identity::{Identity, PublicKey};
use crate::pairing::{self, PairCommand, PairOut};
use crate::peers::{self, MAX_PEERS, Peer};
use crate::session::{self, Ended, Report, ToSession, close, peer_key};
use crate::tls::{self, ALPN, Accept, SERVER_NAME};
use crate::{Command, Config, Event, PairFailure, PeerState, PeerStatus, Refusal, Status, Timings};

/// Accepted handshakes per second on the main endpoint, and the burst allowed.
const HANDSHAKES_PER_SECOND: f64 = 10.0;
/// How often status is sent even when nothing changed, and suspend is looked for.
const TICK: Duration = Duration::from_secs(1);
/// A jump this large between the boot clock and the monotonic one means the machine slept.
const RESUMED: Duration = Duration::from_secs(1);

pub(crate) fn run(
    config: Config,
    identity: Identity,
    peers: Vec<Peer>,
    commands: mpsc::UnboundedReceiver<Command>,
    mut events: Box<dyn FnMut(Event) + Send>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            events(Event::Status(Status {
                listening: Err(format!("no runtime: {e}")),
                fingerprint: identity.public().fingerprint(),
                peers: Vec::new(),
                pairing: false,
            }));
            return;
        }
    };
    runtime.block_on(Engine::new(config, identity, peers, events).run(commands));
}

/// What tasks the engine spawned tell it.
enum Internal {
    /// A dial attempt to `key` ended: with a session, or without and perhaps refused; and the
    /// addresses the name resolved to, if it resolved in time.
    Dialed {
        key: PublicKey,
        conn: Option<Connection>,
        refused: Option<Refusal>,
        resolved: Option<Vec<SocketAddr>>,
    },
    /// A handshake on the main endpoint completed.
    Answered(Connection),
    Pairing(PairOut),
}

struct SessionEntry {
    key: PublicKey,
    dialed: bool,
    conn: Connection,
    to: mpsc::UnboundedSender<ToSession>,
    /// Set once its `Hello` arrived.
    run: Option<u64>,
}

#[derive(Default)]
struct Slot {
    /// The session kept.
    current: Option<u64>,
    /// Sessions handshaken but without a `Hello` yet.
    pending: usize,
    dialing: bool,
    /// When to dial next, if not connected and not dialing.
    next_dial: Option<Instant>,
    backoff: Duration,
    refused: Option<Refusal>,
    suspect: bool,
    rtt: Option<Duration>,
    connects: u32,
    /// When the current session was kept.
    since: Option<Instant>,
    /// The addresses the peer's name last resolved to, for when resolving stalls.
    resolved: Vec<SocketAddr>,
}

struct Pairing {
    to: mpsc::UnboundedSender<PairCommand>,
    address: crate::Address,
}

struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn take(&mut self) -> bool {
        let now = Instant::now();
        let refill = now.duration_since(self.at).as_secs_f64() * HANDSHAKES_PER_SECOND;
        self.tokens = (self.tokens + refill).min(HANDSHAKES_PER_SECOND);
        self.at = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

struct Engine {
    identity: Arc<Identity>,
    own: PublicKey,
    port: u16,
    name: String,
    run: u64,
    timings: Timings,
    peers_path: std::path::PathBuf,
    peers: Vec<Peer>,
    endpoint: Option<Endpoint>,
    bind_error: Option<String>,
    next_bind: Instant,
    slots: HashMap<PublicKey, Slot>,
    sessions: HashMap<u64, SessionEntry>,
    next_id: u64,
    active: HashSet<PublicKey>,
    pairing: Option<Pairing>,
    bucket: Bucket,
    events: Box<dyn FnMut(Event) + Send>,
    tasks: JoinSet<()>,
    internal: mpsc::UnboundedSender<Internal>,
    internal_rx: mpsc::UnboundedReceiver<Internal>,
    reports: mpsc::UnboundedSender<(u64, Report)>,
    reports_rx: mpsc::UnboundedReceiver<(u64, Report)>,
    last_status: Option<Status>,
    next_tick: Instant,
    sleep_gap: Option<Duration>,
}

fn random_u64() -> u64 {
    let mut b = [0u8; 8];
    let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b);
    u64::from_be_bytes(b)
}

/// The boot clock ahead of the monotonic one: it grows by however long the machine slept.
fn sleep_gap() -> Option<Duration> {
    let boot = clock_gettime(ClockId::Boottime);
    let mono = clock_gettime(ClockId::Monotonic);
    let boot = Duration::new(u64::try_from(boot.tv_sec).ok()?, u32::try_from(boot.tv_nsec).ok()?);
    let mono = Duration::new(u64::try_from(mono.tv_sec).ok()?, u32::try_from(mono.tv_nsec).ok()?);
    boot.checked_sub(mono)
}

impl Engine {
    fn new(
        config: Config,
        identity: Identity,
        peers: Vec<Peer>,
        events: Box<dyn FnMut(Event) + Send>,
    ) -> Engine {
        let (internal, internal_rx) = mpsc::unbounded_channel();
        let (reports, reports_rx) = mpsc::unbounded_channel();
        let own = identity.public();
        let now = Instant::now();
        let mut engine = Engine {
            identity: Arc::new(identity),
            own,
            port: config.port,
            name: config.name,
            run: random_u64(),
            peers_path: config.state_dir.join("peers"),
            peers: Vec::new(),
            endpoint: None,
            bind_error: None,
            next_bind: now,
            slots: HashMap::new(),
            sessions: HashMap::new(),
            next_id: 0,
            active: HashSet::new(),
            pairing: None,
            bucket: Bucket { tokens: HANDSHAKES_PER_SECOND, at: now },
            events,
            tasks: JoinSet::new(),
            internal,
            internal_rx,
            reports,
            reports_rx,
            last_status: None,
            next_tick: now,
            sleep_gap: sleep_gap(),
            timings: config.timings,
        };
        for p in peers {
            engine.add_peer(p);
        }
        engine
    }

    fn hello(&self) -> Message {
        Message::Hello { version: link::VERSION, run: self.run, name: self.name.clone() }
    }

    /// Adds a peer, or replaces its record when it is paired again. A slot already there keeps its
    /// sessions and dials; only its backoff, refusal and resolved addresses start over.
    fn add_peer(&mut self, peer: Peer) {
        let first = self.timings.backoff_first;
        let next_dial = Instant::now() + self.first_wait(peer.key);
        let slot = self.slots.entry(peer.key).or_default();
        slot.backoff = first;
        slot.refused = None;
        slot.resolved.clear();
        if slot.current.is_none() && !slot.dialing {
            slot.next_dial = Some(next_dial);
        }
        self.peers.retain(|p| p.key != peer.key);
        self.peers.push(peer);
    }

    /// How long after losing a session to dial: at once for the smaller key, a little later for
    /// the larger, so the session the smaller key dials usually comes up alone.
    fn first_wait(&self, peer: PublicKey) -> Duration {
        if self.own < peer { Duration::ZERO } else { self.timings.defer_dial }
    }

    fn pinned(&self) -> Accept {
        Accept::Pinned(self.peers.iter().map(|p| p.key).collect::<Vec<_>>().into())
    }

    fn server_config(&self) -> std::io::Result<quinn::ServerConfig> {
        tls::server_config(&self.identity, self.pinned(), ALPN, &self.timings)
    }

    fn bind(&mut self) {
        if self.endpoint.is_some() || Instant::now() < self.next_bind {
            return;
        }
        let made = self
            .server_config()
            .and_then(|server| tls::endpoint(&self.identity, self.port, Some(server)));
        match made {
            Ok(e) => {
                self.endpoint = Some(e);
                self.bind_error = None;
            }
            Err(e) => {
                self.bind_error = Some(e.to_string());
                self.next_bind = Instant::now() + self.timings.bind_retry;
            }
        }
    }

    fn refresh_server_config(&self) {
        if let (Some(e), Ok(c)) = (&self.endpoint, self.server_config()) {
            e.set_server_config(Some(c));
        }
    }

    fn status(&self) -> Status {
        let listening = match (&self.endpoint, &self.bind_error) {
            (Some(e), _) => e.local_addr().map(|a| a.port()).map_err(|e| e.to_string()),
            (None, Some(why)) => Err(why.clone()),
            (None, None) => Err("not yet bound".to_owned()),
        };
        let peers = self
            .peers
            .iter()
            .map(|p| {
                let slot = self.slots.get(&p.key);
                let state = match slot {
                    Some(s) if s.current.is_some() && s.suspect => PeerState::Suspect,
                    Some(s) if s.current.is_some() => PeerState::Connected,
                    Some(Slot { refused: Some(r), .. }) => PeerState::Refused(*r),
                    _ => PeerState::Dialing,
                };
                PeerStatus {
                    key: p.key,
                    name: p.name.clone(),
                    address: p.address.to_string(),
                    state,
                    rtt: slot.and_then(|s| s.current.and(s.rtt)),
                    connects: slot.map_or(0, |s| s.connects),
                }
            })
            .collect();
        Status {
            listening,
            fingerprint: self.own.fingerprint(),
            peers,
            pairing: self.pairing.is_some(),
        }
    }

    /// Sends status if it changed, or if a tick is due. A new round trip alone waits for the tick:
    /// an active session measures one every 100 ms.
    fn send_status(&mut self) {
        let now = Instant::now();
        let status = self.status();
        let without_rtt = |s: &Status| {
            let mut s = s.clone();
            for p in &mut s.peers {
                p.rtt = None;
            }
            s
        };
        let changed = self.last_status.as_ref().map(without_rtt) != Some(without_rtt(&status));
        if changed || now >= self.next_tick {
            self.next_tick = now + TICK;
            self.last_status = Some(status.clone());
            (self.events)(Event::Status(status));
        }
    }

    /// Looks for a resume from suspend; after one, every peer is dialed again at once.
    fn check_resumed(&mut self) {
        let gap = sleep_gap();
        if let (Some(before), Some(now)) = (self.sleep_gap, gap)
            && now.saturating_sub(before) >= RESUMED
        {
            let first = self.timings.backoff_first;
            for slot in self.slots.values_mut() {
                slot.backoff = first;
                if slot.next_dial.is_some() {
                    slot.next_dial = Some(Instant::now());
                }
            }
        }
        self.sleep_gap = gap;
    }

    fn dial_due(&mut self) {
        let Some(endpoint) = self.endpoint.clone() else { return };
        let now = Instant::now();
        let due: Vec<PublicKey> = self
            .slots
            .iter()
            .filter(|(_, s)| {
                s.current.is_none()
                    && !s.dialing
                    && s.pending == 0
                    && s.next_dial.is_some_and(|t| t <= now)
            })
            .map(|(k, _)| *k)
            .collect();
        for key in due {
            let Some(peer) = self.peers.iter().find(|p| p.key == key) else { continue };
            let address = peer.address.clone();
            let client = tls::client_config(&self.identity, self.pinned(), ALPN, &self.timings);
            let retry = now + self.timings.backoff_max;
            let Some(slot) = self.slots.get_mut(&key) else { continue };
            let Ok(client) = client else {
                // Not expected; try again later rather than at once, forever.
                slot.next_dial = Some(retry);
                continue;
            };
            slot.dialing = true;
            slot.next_dial = None;
            let cached = slot.resolved.clone();
            let port = address.port_or(self.port);
            let deadline = self.timings.dial_deadline;
            let internal = self.internal.clone();
            let endpoint = endpoint.clone();
            self.tasks.spawn(async move {
                // getaddrinfo can stall for many seconds (DNS, mDNS): bounded, with the last good
                // answer to fall back on.
                let resolved =
                    timeout(deadline, address.resolve(port)).await.ok().filter(|r| !r.is_empty());
                let targets = resolved.clone().unwrap_or(cached);
                let (mut conn, mut refused) = (None, None);
                for addr in targets {
                    let Ok(connecting) = endpoint.connect_with(client.clone(), addr, SERVER_NAME)
                    else {
                        continue;
                    };
                    match timeout(deadline, connecting).await {
                        Ok(Ok(c)) => {
                            conn = Some(c);
                            break;
                        }
                        Ok(Err(e)) => {
                            if let Ended::Refused(r) = session::ended(&e) {
                                refused = Some(r);
                                break;
                            }
                        }
                        Err(_) => {}
                    }
                }
                let _ = internal.send(Internal::Dialed { key, conn, refused, resolved });
            });
        }
    }

    /// Schedules the next dial to `key` after a failure: the backoff, doubled for next time, up to
    /// its ceiling (higher for a refusal).
    fn redial_later(&mut self, key: PublicKey, refused: Option<Refusal>) {
        let (max, first) = (self.timings.backoff_max, self.timings.backoff_first);
        let refused_max = self.timings.refused_max;
        let first_wait = self.first_wait(key);
        let Some(slot) = self.slots.get_mut(&key) else { return };
        if slot.current.is_some() || slot.dialing || slot.pending > 0 {
            return;
        }
        if refused.is_some() {
            slot.refused = refused;
        }
        let ceiling = if slot.refused.is_some() { refused_max } else { max };
        let wait = slot.backoff.max(first_wait);
        slot.next_dial = Some(Instant::now() + wait);
        slot.backoff = (slot.backoff * 2).min(ceiling).max(first);
    }

    fn spawn_session(&mut self, conn: Connection, key: PublicKey, dialed: bool) {
        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        let (to, commands) = mpsc::unbounded_channel();
        if let Some(slot) = self.slots.get_mut(&key) {
            slot.pending += 1;
        }
        self.sessions.insert(id, SessionEntry { key, dialed, conn: conn.clone(), to, run: None });
        self.tasks.spawn(session::run(session::Args {
            id,
            conn,
            dialed,
            hello: self.hello(),
            active: self.active.contains(&key),
            timings: self.timings.clone(),
            reports: self.reports.clone(),
            commands,
        }));
    }

    fn on_internal(&mut self, internal: Internal) {
        match internal {
            Internal::Dialed { key, conn, refused, resolved } => {
                if let Some(slot) = self.slots.get_mut(&key) {
                    slot.dialing = false;
                    if let Some(r) = resolved {
                        slot.resolved = r;
                    }
                }
                if let Some(conn) = conn {
                    // The verifier pinned every peer: the key proved may be another peer's, if
                    // addresses moved. The session belongs to whoever proved their key.
                    match peer_key(&conn).filter(|k| self.slots.contains_key(k)) {
                        Some(proved) => self.spawn_session(conn, proved, true),
                        None => conn.close(close::UNPAIRED, b""),
                    }
                }
                // Does nothing if a session with `key` is up or on its way.
                self.redial_later(key, refused);
            }
            Internal::Answered(conn) => {
                match peer_key(&conn).filter(|k| self.slots.contains_key(k)) {
                    Some(key) => self.spawn_session(conn, key, false),
                    None => conn.close(close::UNPAIRED, b""),
                }
            }
            Internal::Pairing(out) => self.on_pairing(out),
        }
    }

    fn on_incoming(&mut self, incoming: Incoming) {
        // A Retry first, before any token is spent: a forged source address never answers it, so
        // forged packets cannot use up the handshakes real peers need.
        if !incoming.remote_address_validated() && incoming.may_retry() {
            let _ = incoming.retry();
            return;
        }
        if !self.bucket.take() {
            incoming.ignore();
            return;
        }
        let Ok(connecting) = incoming.accept() else { return };
        let internal = self.internal.clone();
        let deadline = self.timings.dial_deadline;
        self.tasks.spawn(async move {
            if let Ok(Ok(conn)) = timeout(deadline, connecting).await {
                let _ = internal.send(Internal::Answered(conn));
            }
        });
    }

    fn on_report(&mut self, id: u64, report: &Report) {
        match report {
            Report::Up { key, run } => self.on_up(id, *key, *run),
            Report::Rtt(rtt) => {
                if let Some(slot) = self.current_slot(id) {
                    slot.rtt = Some(*rtt);
                }
            }
            Report::Suspect(suspect) => {
                if let Some(slot) = self.current_slot(id) {
                    slot.suspect = *suspect;
                }
            }
            Report::Down(ended) => self.on_down(id, *ended),
        }
    }

    fn current_slot(&mut self, id: u64) -> Option<&mut Slot> {
        let key = self.sessions.get(&id)?.key;
        self.slots.get_mut(&key).filter(|s| s.current == Some(id))
    }

    fn close_session(&self, id: u64, code: quinn::VarInt) {
        if let Some(s) = self.sessions.get(&id) {
            s.conn.close(code, b"");
        }
    }

    /// A session's `Hello` arrived: keep it, or the one already kept (docs/link.md, "Who dials").
    fn on_up(&mut self, id: u64, key: PublicKey, run: u64) {
        let Some(entry) = self.sessions.get_mut(&id) else { return };
        entry.run = Some(run);
        let dialed = entry.dialed;
        let Some(slot) = self.slots.get(&key) else {
            self.close_session(id, close::UNPAIRED);
            return;
        };
        let old = slot.current.and_then(|c| self.sessions.get(&c).map(|s| (c, s.run)));
        // Sessions that came up within this long of each other are the two of a simultaneous
        // dial. A later one means the peer lost the old session: it dials only without one.
        let young = slot.since.is_some_and(|t| t.elapsed() < self.timings.dial_deadline * 2);
        let keep_new = match old {
            None => true,
            // Another run of the peer: it restarted, and the old session is dead.
            Some((_, old_run)) if old_run != Some(run) => true,
            // The peer dialed again long after: its side of the old session is gone.
            Some(_) if !young => true,
            // Both machines dialed: keep the session the smaller key dialed.
            Some(_) => dialed == (self.own < key),
        };
        if !keep_new {
            self.close_session(id, close::DUPLICATE);
            if let Some(slot) = self.slots.get_mut(&key) {
                slot.pending = slot.pending.saturating_sub(1);
            }
            return;
        }
        if let Some((old, old_run)) = old {
            let code = if old_run == Some(run) { close::DUPLICATE } else { close::REPLACED };
            self.close_session(old, code);
        }
        let first = self.timings.backoff_first;
        if let Some(slot) = self.slots.get_mut(&key) {
            slot.pending = slot.pending.saturating_sub(1);
            slot.current = Some(id);
            slot.since = Some(Instant::now());
            slot.connects = slot.connects.saturating_add(1);
            slot.next_dial = None;
            slot.backoff = first;
            slot.refused = None;
            slot.suspect = false;
            slot.rtt = None;
        }
        if self.active.contains(&key)
            && let Some(s) = self.sessions.get(&id)
        {
            let _ = s.to.send(ToSession::Active(true));
        }
    }

    fn on_down(&mut self, id: u64, ended: Ended) {
        let Some(entry) = self.sessions.remove(&id) else { return };
        let key = entry.key;
        let first_wait = self.first_wait(key);
        let first = self.timings.backoff_first;
        let Some(slot) = self.slots.get_mut(&key) else { return };
        if entry.run.is_none() {
            slot.pending = slot.pending.saturating_sub(1);
        }
        if slot.current == Some(id) {
            slot.current = None;
            slot.suspect = false;
            slot.rtt = None;
            match ended {
                Ended::Refused(r) => {
                    slot.refused = Some(r);
                    self.redial_later(key, Some(r));
                }
                Ended::Lost | Ended::Closed => {
                    slot.backoff = first;
                    slot.next_dial = Some(Instant::now() + first_wait);
                }
            }
        } else if slot.current.is_none() {
            let refused = match ended {
                Ended::Refused(r) => Some(r),
                Ended::Lost | Ended::Closed => None,
            };
            self.redial_later(key, refused);
        }
    }

    fn on_command(&mut self, command: Command) {
        match command {
            Command::Pair(address) => self.start_pairing(address),
            Command::Confirm(yes) => {
                if let Some(p) = &self.pairing {
                    let _ = p.to.send(PairCommand::Confirm(yes));
                }
            }
            Command::CancelPairing => {
                if let Some(p) = &self.pairing {
                    let _ = p.to.send(PairCommand::Cancel);
                }
            }
            Command::Unpair(key) => self.unpair(key),
            Command::SetActive(key, on) => {
                if on {
                    self.active.insert(key);
                } else {
                    self.active.remove(&key);
                }
                if let Some(id) = self.slots.get(&key).and_then(|s| s.current)
                    && let Some(s) = self.sessions.get(&id)
                {
                    let _ = s.to.send(ToSession::Active(on));
                }
            }
            Command::Shutdown => {}
        }
    }

    fn start_pairing(&mut self, address: crate::Address) {
        if self.pairing.is_some() {
            (self.events)(Event::PairFailed(PairFailure::Busy));
            return;
        }
        let (to, commands) = mpsc::unbounded_channel();
        let (out, mut out_rx) = mpsc::unbounded_channel();
        let internal = self.internal.clone();
        self.tasks.spawn(async move {
            while let Some(o) = out_rx.recv().await {
                let _ = internal.send(Internal::Pairing(o));
            }
        });
        self.tasks.spawn(pairing::run(pairing::Args {
            identity: Arc::clone(&self.identity),
            port: self.port,
            address: address.clone(),
            hello: self.hello(),
            timings: self.timings.clone(),
            commands,
            out,
        }));
        self.pairing = Some(Pairing { to, address });
    }

    fn on_pairing(&mut self, out: PairOut) {
        match out {
            PairOut::Code { name, fingerprint, code } => {
                (self.events)(Event::PairCode { name, fingerprint, code });
            }
            PairOut::Done(result) => {
                let Some(pairing) = self.pairing.take() else { return };
                let event = match result {
                    Err(f) => Event::PairFailed(f),
                    // Checked here, not at the start: pairing a peer again replaces its record.
                    Ok((key, _))
                        if self.peers.len() >= MAX_PEERS && !self.slots.contains_key(&key) =>
                    {
                        Event::PairFailed(PairFailure::TooManyPeers)
                    }
                    Ok((key, name)) => {
                        let peer = Peer { key, address: pairing.address, name: name.clone() };
                        let mut next: Vec<Peer> =
                            self.peers.iter().filter(|p| p.key != key).cloned().collect();
                        next.push(peer.clone());
                        if let Err(e) = peers::save(&self.peers_path, &next) {
                            Event::PairFailed(PairFailure::Save(e.to_string()))
                        } else {
                            self.add_peer(peer);
                            self.refresh_server_config();
                            Event::Paired { name, fingerprint: key.fingerprint() }
                        }
                    }
                };
                (self.events)(event);
            }
        }
    }

    fn unpair(&mut self, key: PublicKey) {
        let fingerprint = key.fingerprint();
        if !self.peers.iter().any(|p| p.key == key) {
            (self.events)(Event::Unpaired { fingerprint, saved: Err("not paired".to_owned()) });
            return;
        }
        let next: Vec<Peer> = self.peers.iter().filter(|p| p.key != key).cloned().collect();
        if let Err(e) = peers::save(&self.peers_path, &next) {
            (self.events)(Event::Unpaired { fingerprint, saved: Err(e.to_string()) });
            return;
        }
        self.peers = next;
        self.slots.remove(&key);
        self.active.remove(&key);
        for s in self.sessions.values().filter(|s| s.key == key) {
            s.conn.close(close::UNPAIRED, b"");
        }
        self.refresh_server_config();
        (self.events)(Event::Unpaired { fingerprint, saved: Ok(()) });
    }

    /// The soonest moment something is due: a dial, a bind retry, or the status tick.
    fn next_wake(&self) -> Instant {
        let mut wake = self.next_tick;
        if self.endpoint.is_none() {
            // No dial can start until the port is bound: wake for the bind, not the dials.
            return wake.min(self.next_bind);
        }
        let dials =
            self.slots.values().filter(|s| !s.dialing && s.pending == 0 && s.current.is_none());
        for t in dials.filter_map(|s| s.next_dial) {
            wake = wake.min(t);
        }
        wake
    }

    async fn run(mut self, mut commands: mpsc::UnboundedReceiver<Command>) {
        loop {
            self.bind();
            self.dial_due();
            self.send_status();
            let wake = self.next_wake();
            let endpoint = self.endpoint.clone();
            let accept = async {
                match &endpoint {
                    Some(e) => e.accept().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                command = commands.recv() => match command {
                    None | Some(Command::Shutdown) => break,
                    Some(c) => self.on_command(c),
                },
                Some((id, report)) = self.reports_rx.recv() => self.on_report(id, &report),
                Some(internal) = self.internal_rx.recv() => self.on_internal(internal),
                incoming = accept => {
                    if let Some(i) = incoming {
                        self.on_incoming(i);
                    } else {
                        self.endpoint = None;
                        self.bind_error = Some("endpoint closed".to_owned());
                    }
                }
                Some(joined) = self.tasks.join_next() => {
                    // A task's panic is not contained: it ends the link thread (docs/link.md).
                    if let Err(e) = joined
                        && e.is_panic()
                    {
                        std::panic::resume_unwind(e.into_panic());
                    }
                }
                () = sleep_until(wake) => {
                    if Instant::now() >= self.next_tick {
                        self.check_resumed();
                    }
                }
            }
        }
        self.shut_down().await;
    }

    /// Says goodbye on every session and closes the endpoint, waiting a little for the goodbyes to
    /// leave.
    async fn shut_down(&mut self) {
        if let Some(p) = &self.pairing {
            let _ = p.to.send(PairCommand::Cancel);
        }
        for s in self.sessions.values() {
            let _ = s.to.send(ToSession::Bye);
        }
        let _ = timeout(Duration::from_millis(500), async {
            while !self.sessions.is_empty() {
                match self.reports_rx.recv().await {
                    Some((id, Report::Down(_))) => {
                        self.sessions.remove(&id);
                    }
                    Some(_) => {}
                    None => break,
                }
            }
        })
        .await;
        if let Some(e) = &self.endpoint {
            e.close(close::BYE, b"");
            let _ = timeout(Duration::from_millis(500), e.wait_idle()).await;
        }
        self.tasks.abort_all();
    }
}
