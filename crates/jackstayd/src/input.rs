//! The input thread (docs/input.md, "In the daemon"): its own X connection, a blocking poll loop
//! over it and a wake pipe, and one of two roles. As the primary it puts barriers on the sides
//! with a neighbour, grabs on a crossing and sends what the user does; as a secondary it injects
//! what its primary sends. The link's reports reach it straight from the link thread, and what it
//! sends goes straight to the link: a keypress never waits behind the clipboard.
//!
//! A dedicated watchdog shuts its X connection down if it holds the grab and stops checking in,
//! which releases the grab; the thread then fails, and the daemon exits for systemd to restart it.

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use jackstay_input::{Do, Layout, Primary, Secondary, Sides, Where};
use jackstay_net::{Heard, Input as Report, PublicKey, Sender, Sink};
use jackstay_proto::link::{Message, Side};
use jackstay_x11_input::{Display, Event, Grab, KeyboardMap, Watchdog};
use rustix::event::{PollFd, PollFlags, Timespec, poll};

use crate::config;

/// Holding the grab and silent this long, the input thread is taken for wedged (docs/input.md,
/// "Watchdog").
pub const WATCHDOG_LIMIT: Duration = Duration::from_millis(500);
/// How often a rival (Synergy and its kind) is looked for.
const RIVAL_CHECK_EVERY: Duration = Duration::from_secs(10);
/// The longest the loop sleeps: it checks the session's freshness and feeds the watchdog.
const TICK: Duration = Duration::from_millis(50);

/// What the clipboard thread tells the input thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// The paired peers, by key and name: neighbours are resolved against them.
    Peers(Vec<(PublicKey, String)>),
    /// `jackstay switch`: to the peer, or home.
    Switch(Option<PublicKey>),
}

/// Input sharing's state, for `jackstay status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Not running, and why.
    Off(String),
    /// Running, but no neighbour can be crossed to (or drive this machine) yet, and why.
    Waiting(String),
    /// The primary, with its pointer here.
    Here,
    /// The primary, with its pointer on `peer`.
    On { peer: PublicKey, locked: bool },
    /// A secondary, waiting for its primary, or driven by it.
    Secondary { primary: PublicKey, driven: bool },
}

/// What the thread publishes for the clipboard thread: its state, and the neighbours it resolved.
#[derive(Debug)]
struct Published {
    state: State,
    neighbours: Vec<PublicKey>,
}

/// The ends the link needs before input's thread starts: the link starts first, and hands its
/// reports to [`Plumbing::sink`].
pub struct Plumbing {
    sink: Sink,
    reports: Receiver<Report>,
    commands_tx: mpsc::Sender<Command>,
    commands: Receiver<Command>,
    wake: UnixStream,
    wake_tx: Arc<UnixStream>,
}

impl std::fmt::Debug for Plumbing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plumbing").finish_non_exhaustive()
    }
}

impl Plumbing {
    /// # Errors
    /// The wake pipe could not be made.
    pub fn new() -> std::io::Result<Plumbing> {
        let (wake, wake_tx) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        wake_tx.set_nonblocking(true)?;
        let wake_tx = Arc::new(wake_tx);
        let (tx, reports) = mpsc::channel();
        let waker = Arc::clone(&wake_tx);
        let sink: Sink = Arc::new(move |r| {
            let _ = tx.send(r);
            // Full means a wake-up is already pending.
            let _ = (&*waker).write(&[1]);
        });
        let (commands_tx, commands) = mpsc::channel();
        Ok(Plumbing { sink, reports, commands_tx, commands, wake, wake_tx })
    }

    #[must_use]
    pub fn sink(&self) -> Sink {
        Arc::clone(&self.sink)
    }
}

/// The clipboard thread's side of the input thread.
#[derive(Debug)]
pub struct InputThread {
    /// How to reach the thread, while it runs.
    to: Option<(mpsc::Sender<Command>, Arc<UnixStream>)>,
    published: Arc<Mutex<Published>>,
    thread: Option<JoinHandle<Result<(), String>>>,
    started: Instant,
    /// The file counting the thread's failures in a row, while it still counts.
    failures: Option<PathBuf>,
}

/// Running this long counts as a success: the failures are forgotten.
const HEALTHY_AFTER: Duration = Duration::from_secs(60);

/// Everything the thread starts with.
#[derive(Debug)]
pub struct Start {
    pub plumbing: Plumbing,
    pub config: config::Input,
    pub display: Option<String>,
    /// The link, if it runs.
    pub link: Option<Sender>,
    /// Counts the thread's failures in a row; removed once it has run for a minute.
    pub failures: Option<PathBuf>,
}

impl InputThread {
    /// Starts the thread. It clears keys left held through `XTest` whether input sharing runs or
    /// not (docs/input.md, "Secondary"), and ends at once, fine, if there is nothing to share.
    ///
    /// # Errors
    /// The thread could not start.
    pub fn start(start: Start) -> std::io::Result<InputThread> {
        let published = Arc::new(Mutex::new(Published {
            state: State::Off("starting".to_owned()),
            neighbours: Vec::new(),
        }));
        let commands = start.plumbing.commands_tx.clone();
        let wake = Arc::clone(&start.plumbing.wake_tx);
        let failures = start.failures.clone();
        let shared = Arc::clone(&published);
        let thread = std::thread::Builder::new()
            .name("input".to_owned())
            .spawn(move || run(start, &shared))?;
        let started = Instant::now();
        let to = Some((commands, wake));
        Ok(InputThread { to, published, thread: Some(thread), started, failures })
    }

    /// Input sharing off from the start, and why: the thread never runs.
    #[must_use]
    pub fn off(why: String) -> InputThread {
        InputThread {
            to: None,
            published: Arc::new(Mutex::new(Published {
                state: State::Off(why),
                neighbours: Vec::new(),
            })),
            thread: None,
            started: Instant::now(),
            failures: None,
        }
    }

    /// Whether the thread still runs, or ended fine: `Err` with why if it failed. A minute of
    /// running forgets earlier failures.
    ///
    /// # Errors
    /// The thread failed.
    pub fn check(&mut self) -> Result<(), String> {
        if self.started.elapsed() >= HEALTHY_AFTER
            && let Some(path) = self.failures.take()
        {
            let _ = std::fs::remove_file(path);
        }
        self.failed().map_or(Ok(()), Err)
    }

    pub fn tell(&self, command: Command) {
        if let Some((commands, wake)) = &self.to {
            let _ = commands.send(command);
            let _ = (&**wake).write(&[1]);
        }
    }

    #[must_use]
    pub fn state(&self) -> State {
        self.published.lock().unwrap_or_else(PoisonError::into_inner).state.clone()
    }

    /// The neighbours, as resolved to peers.
    #[must_use]
    pub fn neighbours(&self) -> Vec<PublicKey> {
        self.published.lock().unwrap_or_else(PoisonError::into_inner).neighbours.clone()
    }

    /// Why the thread failed, once it has: its X connection broke (the watchdog shuts it down),
    /// or it panicked. `None` while it runs, and if it ended fine (nothing to share).
    fn failed(&mut self) -> Option<String> {
        if !self.thread.as_ref().is_some_and(JoinHandle::is_finished) {
            return None;
        }
        match self.thread.take()?.join() {
            Ok(Ok(())) => None,
            Ok(Err(why)) => Some(why),
            Err(_) => Some("the input thread panicked".to_owned()),
        }
    }
}

fn set(shared: &Mutex<Published>, state: State, neighbours: &[Option<PublicKey>]) {
    let mut p = shared.lock().unwrap_or_else(PoisonError::into_inner);
    p.state = state;
    p.neighbours = neighbours.iter().flatten().copied().collect();
}

fn log(line: &str) {
    let _ = writeln!(std::io::stderr(), "{line}");
}

fn x(e: impl std::fmt::Display) -> String {
    format!("input: {e}")
}

fn run(start: Start, shared: &Mutex<Published>) -> Result<(), String> {
    let Start { plumbing, config, display, link, .. } = start;
    let mut display = Display::connect(display.as_deref()).map_err(x)?;
    let cleared = display.clear_xtest().map_err(x)?;
    if cleared > 0 {
        log(&format!("input: released {cleared} keys and buttons left held through XTest"));
    }
    let named = Side::ALL.into_iter().any(|s| config.neighbour(s).is_some());
    let Some(link) = link.filter(|_| named) else {
        let why = if named { "the link is off" } else { "no neighbours in the configuration" };
        set(shared, State::Off(why.to_owned()), &[]);
        return Ok(());
    };
    let watchdog = Watchdog::start(display.socket().map_err(x)?, WATCHDOG_LIMIT).map_err(x)?;
    if layout(&display).is_err() {
        set(shared, State::Off("no monitor to share".to_owned()), &[]);
        return Ok(());
    }
    let mut worker = Worker::new(&mut display, config, link)?;
    let mut rival_checked: Option<Instant> = None;
    let mut rival: Option<&'static str> = None;
    loop {
        // Not while the pointer is on a peer: reading every process's name would stall its motion.
        if rival_checked.is_none_or(|t| t.elapsed() >= RIVAL_CHECK_EVERY) && !worker.driving() {
            rival_checked = Some(Instant::now());
            let now = crate::coexist::input_rival(std::path::Path::new("/proc"));
            if now.is_some() && rival.is_none() {
                worker.sleep(&mut display)?;
            } else if now.is_none() && rival.is_some() {
                worker.wake(&mut display)?;
            }
            rival = now;
        }
        let mut drained = [0u8; 256];
        while matches!((&plumbing.wake).read(&mut drained), Ok(n) if n > 0) {}
        for command in plumbing.commands.try_iter() {
            worker.command(&mut display, command)?;
        }
        for report in plumbing.reports.try_iter() {
            worker.report(&mut display, report)?;
        }
        // Until none are queued: any request waiting for a reply reads events into the
        // connection's queue, where poll cannot see them.
        loop {
            let events = display.events().map_err(x)?;
            if events.is_empty() {
                break;
            }
            for e in events {
                worker.event(&mut display, e)?;
            }
        }
        worker.tick(&mut display)?;
        let state = rival.map_or_else(|| worker.state(), |r| State::Off(format!("{r} is running")));
        set(shared, state, &worker.resolved);
        watchdog.beat(display.grabbed());
        let (xfd, wakefd) = (display.fd(), plumbing.wake.as_fd());
        let mut fds = [PollFd::new(&xfd, PollFlags::IN), PollFd::new(&wakefd, PollFlags::IN)];
        let timeout = Timespec::try_from(TICK).ok();
        let _ = poll(&mut fds, timeout.as_ref());
    }
}

/// A neighbour named in the configuration, found among the peers: by fingerprint, or by a name
/// only one peer has.
fn resolve(name: &str, peers: &[(PublicKey, String)]) -> Result<PublicKey, String> {
    if let Some((k, _)) = peers.iter().find(|(k, _)| k.fingerprint() == name) {
        return Ok(*k);
    }
    let mut named = peers.iter().filter(|(_, n)| n == name);
    match (named.next(), named.next()) {
        (Some((k, _)), None) => Ok(*k),
        (Some(_), Some(_)) => Err(format!("two paired machines are named {name}")),
        (None, _) => Err(format!("no paired machine is {name}")),
    }
}

/// The neighbours resolved, by side (left, right, top, bottom), and why any is not.
fn neighbours(
    config: &config::Input,
    peers: &[(PublicKey, String)],
) -> ([Option<PublicKey>; 4], Option<String>) {
    let mut out = [None; 4];
    let mut why = None;
    for (slot, side) in out.iter_mut().zip(Side::ALL) {
        if let Some(name) = config.neighbour(side) {
            match resolve(name, peers) {
                Ok(k) => *slot = Some(k),
                Err(e) => why = why.or(Some(e)),
            }
        }
    }
    (out, why)
}

/// One session's state, per peer.
struct Linked {
    session: u64,
    heard: Arc<Heard>,
}

enum Role {
    Primary(Box<Primary<PublicKey>>),
    /// The primary's key once resolved, and the machine it drives.
    Secondary(Option<PublicKey>, Box<Secondary>),
}

struct Worker {
    config: config::Input,
    link: Sender,
    layout: Layout,
    map: KeyboardMap,
    role: Role,
    sessions: HashMap<PublicKey, Linked>,
    peers: Vec<(PublicKey, String)>,
    resolved: [Option<PublicKey>; 4],
    unresolved: Option<String>,
    /// A rival runs: nothing is grabbed, injected, or barred.
    asleep: bool,
    last: Option<Where<PublicKey>>,
    /// When the pointer went onto the peer it is on.
    on_since: Option<Instant>,
}

fn layout(display: &Display) -> Result<Layout, String> {
    let monitors = display.monitors().map_err(x)?;
    Layout::new(&monitors).map_err(|_| "input: no monitor can be shared".to_owned())
}

impl Worker {
    fn new(display: &mut Display, config: config::Input, link: Sender) -> Result<Worker, String> {
        let layout = layout(display)?;
        let map = display.keyboard_map().map_err(x)?;
        let role = if config.primary {
            Role::Primary(Box::new(Primary::new(
                layout.clone(),
                [None; 4],
                (map.first, map.keys.clone()),
                config.return_key,
                config.lock_key,
            )))
        } else {
            let secondary = Secondary::new(layout.clone(), Sides::default(), map.keymap());
            Role::Secondary(None, Box::new(secondary))
        };
        Ok(Worker {
            config,
            link,
            layout,
            map,
            role,
            sessions: HashMap::new(),
            peers: Vec::new(),
            resolved: [None; 4],
            unresolved: None,
            asleep: false,
            last: None,
            on_since: None,
        })
    }

    /// Whether this machine's pointer is on a peer now.
    fn driving(&self) -> bool {
        matches!(&self.role, Role::Primary(p) if matches!(p.position(), Where::On { .. }))
    }

    fn state(&self) -> State {
        match &self.role {
            Role::Primary(p) => match p.position() {
                Where::On { peer, locked } => State::On { peer, locked },
                Where::Here | Where::Crossing(_) => match &self.unresolved {
                    Some(why) => State::Waiting(why.clone()),
                    None => State::Here,
                },
            },
            Role::Secondary(Some(primary), s) => {
                State::Secondary { primary: *primary, driven: s.active() }
            }
            Role::Secondary(None, _) => State::Waiting(
                self.unresolved.clone().unwrap_or_else(|| "waiting for the peers".to_owned()),
            ),
        }
    }

    /// A rival started: everything this thread holds is let go.
    fn sleep(&mut self, display: &mut Display) -> Result<(), String> {
        self.asleep = true;
        match &mut self.role {
            Role::Primary(p) => {
                let out = p.switch_home();
                self.act(display, out)?;
                display.set_barriers(&[]).map_err(x)?;
            }
            Role::Secondary(_, s) => {
                let out = s.end();
                display.inject(&out, &self.map).map_err(x)?;
            }
        }
        log("input: off while a rival runs");
        Ok(())
    }

    fn wake(&mut self, display: &mut Display) -> Result<(), String> {
        self.asleep = false;
        log("input: on again");
        if let Role::Primary(p) = &self.role {
            let barriers = p.barriers();
            self.act(display, vec![barriers])?;
        }
        Ok(())
    }

    fn command(&mut self, display: &mut Display, command: Command) -> Result<(), String> {
        match command {
            Command::Peers(peers) => {
                self.peers = peers;
                let (resolved, why) = neighbours(&self.config, &self.peers);
                self.resolved = resolved;
                self.unresolved = why;
                match &mut self.role {
                    Role::Primary(p) => {
                        let out = p.neighbours(resolved);
                        if !self.asleep {
                            self.act(display, out)?;
                        }
                    }
                    Role::Secondary(primary, s) => {
                        let keys: Vec<PublicKey> = resolved.iter().flatten().copied().collect();
                        let first = keys.first().copied();
                        if keys.iter().any(|k| Some(*k) != first) {
                            self.unresolved =
                                Some("a secondary's neighbours must all be the primary".to_owned());
                        }
                        let exits = Side::ALL
                            .into_iter()
                            .zip(resolved)
                            .filter(|(_, k)| k.is_some() && *k == first)
                            .fold(Sides::default(), |s, (side, _)| s.with(side));
                        s.exits(exits);
                        if *primary != first {
                            // Another primary, or the first: what the old one held goes up, and
                            // the new one, if already linked, learns our monitors.
                            let held = s.end();
                            display.inject(&held, &self.map).map_err(x)?;
                            **s = Secondary::new(self.layout.clone(), exits, self.map.keymap());
                            *primary = first;
                            if let Some(k) = first
                                && let Some(l) = self.sessions.get(&k)
                            {
                                let monitors = self.layout.wire();
                                self.link.send(k, l.session, Message::Layout { monitors });
                            }
                        }
                    }
                }
            }
            Command::Switch(to) => {
                if let Role::Primary(p) = &mut self.role
                    && !self.asleep
                {
                    let out = match to {
                        Some(peer) => p.switch_to(peer),
                        None => p.switch_home(),
                    };
                    self.act(display, out)?;
                }
            }
        }
        Ok(())
    }

    /// What a session of the link reported.
    fn report(&mut self, display: &mut Display, report: Report) -> Result<(), String> {
        match report {
            Report::Kept { key, session, heard } => {
                self.sessions.insert(key, Linked { session, heard });
                match &mut self.role {
                    Role::Primary(p) => {
                        let out = p.kept(key, session);
                        self.act(display, out)?;
                    }
                    Role::Secondary(primary, s) if *primary == Some(key) => {
                        let out = s.reset();
                        display.inject(&out, &self.map).map_err(x)?;
                        self.link.send(
                            key,
                            session,
                            Message::Layout { monitors: self.layout.wire() },
                        );
                    }
                    Role::Secondary(..) => {}
                }
            }
            Report::Message { key, session, message } => {
                self.message(display, key, session, &message)?;
            }
            Report::Suspect { key, session, suspect } => match &mut self.role {
                Role::Primary(p) => {
                    let out = p.suspect(key, session, suspect);
                    self.act(display, out)?;
                }
                Role::Secondary(primary, s) if *primary == Some(key) && suspect => {
                    if self.sessions.get(&key).is_some_and(|l| l.session == session) {
                        // What the primary sent meanwhile may come late, and is dropped until its
                        // `Exit`. A secondary is never the faster pinger: suspect, it has heard
                        // nothing from its primary.
                        let out = s.suspect();
                        display.inject(&out, &self.map).map_err(x)?;
                    }
                }
                Role::Secondary(..) => {}
            },
            Report::Ended { key, session } => {
                if self.sessions.get(&key).is_some_and(|l| l.session == session) {
                    self.sessions.remove(&key);
                }
                match &mut self.role {
                    Role::Primary(p) => {
                        let out = p.ended(key, session);
                        self.act(display, out)?;
                    }
                    Role::Secondary(primary, s) if *primary == Some(key) => {
                        if !self.sessions.contains_key(&key) {
                            let out = s.end();
                            display.inject(&out, &self.map).map_err(x)?;
                        }
                    }
                    Role::Secondary(..) => {}
                }
            }
        }
        Ok(())
    }

    fn message(
        &mut self,
        display: &mut Display,
        key: PublicKey,
        session: u64,
        message: &Message,
    ) -> Result<(), String> {
        let current = self.sessions.get(&key).is_some_and(|l| l.session == session);
        match &mut self.role {
            Role::Primary(p) => {
                let out = p.message(key, session, message);
                self.act(display, out)
            }
            // A secondary takes input only from its primary, on the session kept, and not while a
            // rival runs.
            Role::Secondary(primary, s) if *primary == Some(key) && current && !self.asleep => {
                let actions = match *message {
                    Message::Keymap { first, ref keys } => {
                        s.primary_keymap(first, keys.clone());
                        Vec::new()
                    }
                    Message::Enter { span, side, fraction, ref modifiers, locks } => {
                        // Whatever a crashed run left held goes up before a new span.
                        display.clear_xtest().map_err(x)?;
                        s.enter(span, side, fraction, modifiers, locks)
                    }
                    Message::Motion { span, dx, dy } => s.motion(span, dx, dy),
                    Message::Button { span, button, down } => s.button(span, button, down),
                    Message::Key { span, keycode, down } => s.key(span, keycode, down),
                    Message::Exit { span } => s.exit(span),
                    Message::Lock { span, on } => {
                        s.lock(span, on);
                        Vec::new()
                    }
                    _ => Vec::new(),
                };
                display.inject(&actions, &self.map).map_err(x)?;
                for a in actions {
                    if let jackstay_input::Action::Leave { span, side, fraction } = a {
                        self.link.send(key, session, Message::Leave { span, side, fraction });
                    }
                }
                Ok(())
            }
            Role::Secondary(..) => Ok(()),
        }
    }

    fn event(&mut self, display: &mut Display, event: Event) -> Result<(), String> {
        match event {
            Event::Layout => {
                // None to share (every output off, a monitor powered down): the last ones stand.
                let Ok(layout) = layout(display) else { return Ok(()) };
                if layout == self.layout {
                    return Ok(());
                }
                self.layout = layout.clone();
                match &mut self.role {
                    Role::Primary(p) => {
                        let out = p.layout(layout);
                        // No barriers while a rival runs; waking puts them back.
                        if !self.asleep {
                            self.act(display, out)?;
                        }
                    }
                    Role::Secondary(primary, s) => {
                        let out = s.layout(layout);
                        display.inject(&out, &self.map).map_err(x)?;
                        if let Some(k) = *primary
                            && let Some(l) = self.sessions.get(&k)
                        {
                            self.link.send(
                                k,
                                l.session,
                                Message::Layout { monitors: self.layout.wire() },
                            );
                        }
                    }
                }
            }
            Event::Keymap => {
                self.map = display.keyboard_map().map_err(x)?;
                match &mut self.role {
                    Role::Primary(p) => {
                        let out = p.keymap(self.map.first, self.map.keys.clone());
                        self.act(display, out)?;
                    }
                    Role::Secondary(_, s) => s.keymap(self.map.keymap()),
                }
            }
            _ if self.asleep => {}
            Event::BarrierHit { side, x: px, y: py, dx, dy } => {
                if let Role::Primary(p) = &mut self.role {
                    let out = p.barrier(side, px, py, dx, dy, Instant::now());
                    self.act(display, out)?;
                }
            }
            Event::Motion { dx, dy, xtest } => match &mut self.role {
                Role::Primary(p) => {
                    let out = p.motion(dx, dy);
                    self.act(display, out)?;
                }
                // This machine's own mouse: the primary's motion goes on from where it left
                // the pointer.
                Role::Secondary(_, s) if !xtest && s.active() => {
                    let (px, py, _) = display.pointer().map_err(x)?;
                    s.moved(px, py);
                }
                Role::Secondary(..) => {}
            },
            Event::Button { button, down } => {
                if let Role::Primary(p) = &mut self.role {
                    let out = p.button(button, down);
                    self.act(display, out)?;
                }
            }
            Event::Key { keycode, down } => {
                if let Role::Primary(p) = &mut self.role {
                    let out = p.key(keycode, down);
                    self.act(display, out)?;
                }
            }
        }
        Ok(())
    }

    /// The session the pointer is on, checked for freshness whatever the link thread says; and
    /// crossings and returns said in the journal.
    fn tick(&mut self, display: &mut Display) -> Result<(), String> {
        let Role::Primary(p) = &mut self.role else { return Ok(()) };
        if let Where::On { peer, .. } = p.position()
            && let Some(l) = self.sessions.get(&peer)
        {
            // Measured from the crossing at the latest: an idle session is heard only every
            // second, and its faster pings start with the span.
            let since = *self.on_since.get_or_insert_with(Instant::now);
            let out = p.freshness(l.heard.age().min(since.elapsed()));
            self.act(display, out)?;
        } else {
            self.on_since = None;
        }
        let Role::Primary(p) = &self.role else { return Ok(()) };
        let now = p.position();
        if self.last != Some(now) {
            match now {
                Where::On { peer, locked } => {
                    log(&format!(
                        "input: on {}{}",
                        peer.fingerprint(),
                        if locked { ", locked" } else { "" }
                    ));
                }
                Where::Here if self.last.is_some() => log("input: here"),
                Where::Here | Where::Crossing(_) => {}
            }
            self.last = Some(now);
        }
        Ok(())
    }

    /// Does what the primary says, in order.
    fn act(&mut self, display: &mut Display, todo: Vec<Do<PublicKey>>) -> Result<(), String> {
        for d in todo {
            match d {
                Do::Grab => {
                    let took = display.grab().map_err(x)? == Grab::Grabbed;
                    let (modifiers, locks) = if took {
                        display.held_modifiers(&self.map).map_err(x)?
                    } else {
                        ([0; 32], 0)
                    };
                    if let Role::Primary(p) = &mut self.role {
                        let out = p.grabbed(took, modifiers, locks);
                        self.act(display, out)?;
                    }
                }
                Do::Ungrab => display.ungrab().map_err(x)?,
                Do::Warp { x: px, y: py } => display.warp(px, py).map_err(x)?,
                Do::Barriers(edges) => display.set_barriers(&edges).map_err(x)?,
                Do::Send { peer, session, message } => self.link.send(peer, session, message),
                Do::Active { peer, on } => self.link.set_active(peer, on),
            }
        }
        Ok(())
    }
}

/// Where the input thread's failures in a row are counted (docs/input.md, "Watchdog").
#[must_use]
pub fn failures_file(state: &std::path::Path) -> PathBuf {
    state.join("input-failures")
}
