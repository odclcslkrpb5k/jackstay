//! The clipboard thread (docs/daemon.md, "Threads"): one poll over the X connection, the control
//! socket and its clients, the store thread's replies and the signal pipe, never waiting longer
//! than a second, so the watchdog is fed and the loop always turns. Nothing here waits on the disk
//! or on a client.

use std::collections::{HashMap, VecDeque};
use std::io::{Read as _, Write as _};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use jackstay_clip::EntryId;
use jackstay_proto::control::{
    Error, MAX_LIST, MAX_SOURCE_LEN, PairReason, Request, Response, Status, Thumb, printable,
};
use jackstay_selection::Report;
use jackstay_x11_clip::Clipboard;
use rustix::event::{PollFd, PollFlags, Timespec, poll};

use crate::control::{ClientId, Clients};
use crate::import;
use crate::input::InputThread;
use crate::link::{self, Link};
use crate::notify::Notifier;
use crate::store::{Reply, Request as StoreRequest, Snapshot};
use crate::summary::{summary, truncate};
use crate::thumbnails::Thumbnails;
use crate::tracker::{PinError, Tracker};

/// The longest the loop sleeps, so it always turns (and feeds the watchdog).
const MAX_SLEEP: Duration = Duration::from_secs(1);

/// Why the session ended.
#[derive(Debug)]
pub enum Exit {
    /// `SIGTERM` or `SIGINT`.
    Stopped,
    /// Another clipboard manager owns `CLIPBOARD_MANAGER`, or took it from us (exit status 3).
    AnotherManager,
    /// clipman started while we run (exit status 3).
    Clipman,
    /// The X connection is gone: logout, or the server died (exit status 4).
    XGone(jackstay_x11_clip::Error),
    /// The store thread ended (exit status 101).
    StoreGone,
    /// The link thread ended or stopped reporting (exit status 101).
    LinkGone(link::Gone),
    /// The input thread failed, with why (exit status 101).
    InputGone(String),
}

/// The store thread, as the session talks to it.
#[derive(Debug)]
pub struct StoreLink {
    pub requests: Sender<StoreRequest>,
    pub replies: Receiver<Reply>,
    /// Readable whenever a reply is waiting.
    pub wake: UnixStream,
}

/// A check, run every 30 s, that returns `true` if clipman has started.
pub type ClipmanCheck = Box<dyn FnMut() -> bool + Send>;

const CLIPMAN_CHECK_EVERY: Duration = Duration::from_secs(30);

/// What a session is made of, as `main` sets it up.
pub struct Parts {
    pub clipboard: Clipboard,
    pub tracker: Tracker,
    pub clients: Clients,
    /// `None` when the store could not be read at start: the history is kept in memory only.
    pub store: Option<StoreLink>,
    /// Readable when a stopping signal arrived.
    pub signals: UnixStream,
    pub clipman_dir: PathBuf,
    pub per_target: u64,
    pub clipman_check: ClipmanCheck,
    /// `None` when its thread could not start: no entry has a thumbnail.
    pub thumbnails: Option<Thumbnails>,
    /// `None` when its wake pipe could not be made; otherwise running, or off with a reason.
    pub link: Option<Link>,
    /// Running, or off with a reason.
    pub input: InputThread,
}

impl std::fmt::Debug for Parts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Parts").field("tracker", &self.tracker).finish_non_exhaustive()
    }
}

/// The pairing running now, as far as the session knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    /// Started by this client, which gets its events.
    Client(ClientId),
    /// Its client left and it was cancelled; its last event has not arrived yet. Until it does, a
    /// new pairing is refused, so that event is never taken for the new one's.
    Cancelling,
}

pub struct Session {
    pub clipboard: Clipboard,
    pub tracker: Tracker,
    pub clients: Clients,
    /// `None` when the store could not be read at start: the history is kept in memory only.
    pub store: Option<StoreLink>,
    /// Readable when a stopping signal arrived.
    pub signals: UnixStream,
    pub notifier: Notifier,
    pub clipman_dir: PathBuf,
    pub per_target: u64,
    pub clipman_check: ClipmanCheck,
    pub thumbnails: Option<Thumbnails>,
    pub link: Option<Link>,
    pub input: InputThread,
    /// The peers the input thread was last told of.
    peers_told: Option<Vec<(jackstay_net::PublicKey, String)>>,
    /// The client that started the pairing running now, which gets its events.
    pairing: Option<Flow>,
    /// Clients waiting for an unpairing to be saved, in the order asked.
    unpairing: VecDeque<ClientId>,
    /// Clients waiting for the store to confirm, by acknowledgement number, with the error to give
    /// them even if it does (a clear whose claim failed).
    waiting: HashMap<u64, (ClientId, Option<Error>)>,
    next_ack: u64,
    store_failing: bool,
    last_clipman_check: Instant,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("tracker", &self.tracker)
            .field("clients", &self.clients.len())
            .field("store_failing", &self.store_failing)
            .finish_non_exhaustive()
    }
}

/// Milliseconds since the Unix epoch, for an entry's copy time.
fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// One line to the journal: kinds and counts, never contents (docs/daemon.md, "Logging").
fn log(line: &str) {
    // The journal may be gone (stderr closed); that must not stop the clipboard.
    let _ = writeln!(std::io::stderr(), "{line}");
}

/// Why we cannot run: who holds `CLIPBOARD_MANAGER`, if we could tell, and for Xfce's settings
/// daemon, how to turn its clipboard manager off (docs/daemon.md, "Coexistence").
pub(crate) fn manager_conflict(holder: Option<&str>) -> String {
    let what = "another clipboard manager owns CLIPBOARD_MANAGER";
    // Any client can set its WM_CLASS: cleaned as an entry's source is.
    match holder.map(|h| truncate(&printable(h), MAX_SOURCE_LEN)) {
        Some(h) if h == "xfsettingsd" => format!(
            "{what}: xfsettingsd, whose clipboard manager runs unless it starts with \
             XFSETTINGSD_NO_CLIPBOARD=1; tools/install sets that from the next login, and \
             `XFSETTINGSD_NO_CLIPBOARD=1 xfsettingsd --replace --daemon` sets it now"
        ),
        Some(h) => format!("{what}: {h}"),
        // Most likely still xfsettingsd's, on Xfce.
        None => format!(
            "{what}; on Xfce, xfsettingsd runs one unless started with \
             XFSETTINGSD_NO_CLIPBOARD=1"
        ),
    }
}

fn describe(report: &Report) -> Option<String> {
    Some(match report {
        Report::Captured { capture, content, complete, .. } => format!(
            "capture {capture}: {} targets, {} bytes held{}",
            content.targets().len(),
            content.kept_len(),
            if *complete { "" } else { " so far" }
        ),
        Report::CaptureFailed { capture } => format!("capture {capture}: nothing could be kept"),
        Report::OwnerHung => "the clipboard's owner stopped answering".to_owned(),
        Report::KeptAlive => "the owner exited; serving its last copy".to_owned(),
        Report::ManagerConflict { holder } => manager_conflict(holder.as_deref()),
        Report::ManagerLost => "another client took CLIPBOARD_MANAGER".to_owned(),
        Report::CaptureStarted { .. }
        | Report::CaptureEnded { .. }
        | Report::Claimed
        | Report::ClaimFailed
        | Report::Lost
        | Report::SaveAnswered { .. } => return None,
    })
}

/// Where a polled descriptor came from.
#[derive(Clone, Copy)]
enum Slot {
    X,
    Listener,
    Store,
    Signals,
    Thumbnails,
    Link,
    Client(ClientId),
}

impl Session {
    #[must_use]
    pub fn new(parts: Parts) -> Self {
        let Parts {
            clipboard,
            tracker,
            clients,
            store,
            signals,
            clipman_dir,
            per_target,
            clipman_check,
            mut thumbnails,
            link,
            input,
        } = parts;
        if let Some(store) = &store {
            let _ = store.wake.set_nonblocking(true);
        }
        if let Some(t) = &mut thumbnails {
            t.sync(tracker.history());
        }
        Session {
            clipboard,
            tracker,
            clients,
            store,
            signals,
            notifier: Notifier::from_env(),
            clipman_dir,
            per_target,
            clipman_check,
            thumbnails,
            link,
            input,
            peers_told: None,
            pairing: None,
            unpairing: VecDeque::new(),
            waiting: HashMap::new(),
            next_ack: 0,
            store_failing: false,
            last_clipman_check: Instant::now(),
        }
    }

    /// Applies reports from the engine. Returns `Err` if one ends the session.
    fn reports(&mut self, reports: &[Report]) -> Result<bool, Exit> {
        let now = wall_ms();
        let mut changed = false;
        for r in reports {
            if let Some(line) = describe(r) {
                log(&line);
            }
            if matches!(r, Report::ManagerConflict { .. } | Report::ManagerLost) {
                return Err(Exit::AnotherManager);
            }
            changed |= self.tracker.on_report(r, now);
        }
        Ok(changed)
    }

    /// After any change to the history: its thumbnails brought up to date, and subscribers told.
    fn changed(&mut self) {
        if let Some(t) = &mut self.thumbnails {
            t.sync(self.tracker.history());
        }
        self.clients.changed();
    }

    /// Saves the history, and tells subscribers it changed.
    fn save(&mut self) {
        self.changed();
        if let Some(store) = &self.store {
            let snapshot = Snapshot::of(self.tracker.history(), self.tracker.cleared());
            let _ = store.requests.send(StoreRequest::Save { snapshot, urgent: false, ack: None });
        }
    }

    /// Saves urgently, answering `client` once the store confirms; without a store, at once.
    fn save_urgently(&mut self, client: ClientId, clear_history: bool, failure: Option<Error>) {
        self.changed();
        let Some(store) = &self.store else {
            self.clients.respond(client, &Response::Error(Error::StoreFailed));
            return;
        };
        let ack = self.next_ack;
        self.next_ack = self.next_ack.wrapping_add(1);
        let snapshot = Snapshot::of(self.tracker.history(), self.tracker.cleared());
        let request = if clear_history {
            StoreRequest::ClearHistory { snapshot, ack }
        } else {
            StoreRequest::Save { snapshot, urgent: true, ack: Some(ack) }
        };
        if store.requests.send(request).is_ok() {
            self.waiting.insert(ack, (client, failure));
            self.clients.defer(client);
        } else {
            self.clients.respond(client, &Response::Error(Error::StoreFailed));
        }
    }

    /// Clears the clipboard (and PRIMARY), deleting the entry on it unless pinned. Returns the
    /// error for the client if the claim failed: another application holds the clipboard, and what
    /// it holds was not cleared.
    fn clear_clipboard(&mut self) -> Result<Option<Error>, Exit> {
        let reports = self.clipboard.clear().map_err(Exit::XGone)?;
        self.reports(&reports)?;
        self.tracker.clipboard_cleared();
        if reports.contains(&Report::Claimed) {
            Ok(None)
        } else {
            log("clear: another client holds the clipboard");
            Ok(Some(Error::ClaimFailed))
        }
    }

    fn status(&self) -> Status {
        let history = self.tracker.history();
        Status {
            entries: u32::try_from(history.entries().len()).unwrap_or(u32::MAX),
            pinned: u32::try_from(history.entries().filter(|e| e.pinned()).count())
                .unwrap_or(u32::MAX),
            current: self.tracker.current().map(EntryId::get),
            owns_clipboard: self.clipboard.owns_clipboard(),
            store_failing: self.store_failing || self.store.is_none(),
        }
    }

    fn request(&mut self, client: ClientId, request: Request) -> Result<(), Exit> {
        let respond = |s: &mut Self, r: Response| s.clients.respond(client, &r);
        match request {
            Request::Status => {
                let status = self.status();
                respond(self, Response::Status(status));
            }
            Request::List { offset, limit } => {
                let current = self.tracker.current();
                let history = self.tracker.history();
                let limit = usize::try_from(limit).unwrap_or(usize::MAX).min(MAX_LIST);
                let entries = history
                    .entries()
                    .skip(usize::try_from(offset).unwrap_or(usize::MAX))
                    .take(limit)
                    .map(|e| summary(e, Some(e.id()) == current))
                    .collect();
                let total = u32::try_from(history.entries().len()).unwrap_or(u32::MAX);
                respond(self, Response::List { total, entries });
            }
            Request::Select { id } => {
                let id = EntryId::new(id);
                let Some(content) = self.tracker.select(id) else {
                    respond(self, Response::Error(Error::NoSuchEntry));
                    return Ok(());
                };
                let reports = self.clipboard.serve(content).map_err(Exit::XGone)?;
                self.reports(&reports)?;
                let claimed = reports.contains(&Report::Claimed);
                self.tracker.served(id, claimed);
                self.save();
                respond(
                    self,
                    if claimed { Response::Ok } else { Response::Error(Error::ClaimFailed) },
                );
            }
            Request::Delete { id } => match self.tracker.delete(EntryId::new(id)) {
                None => respond(self, Response::Error(Error::NoSuchEntry)),
                Some(was_current) => {
                    let failure = if was_current { self.clear_clipboard()? } else { None };
                    self.save_urgently(client, false, failure);
                }
            },
            Request::Pin { id, pinned } => match self.tracker.pin(EntryId::new(id), pinned) {
                Ok(()) => {
                    self.save();
                    respond(self, Response::Ok);
                }
                Err(PinError::NoSuchEntry) => respond(self, Response::Error(Error::NoSuchEntry)),
                Err(PinError::TooManyPinned) => {
                    respond(self, Response::Error(Error::TooManyPinned));
                }
            },
            Request::Clear => {
                let failure = self.clear_clipboard()?;
                log("clipboard cleared");
                self.save_urgently(client, false, failure);
            }
            Request::ClearHistory => {
                let removed = self.tracker.clear_history();
                let failure = self.clear_clipboard()?;
                log(&format!("history cleared: {removed} entries"));
                self.save_urgently(client, true, failure);
            }
            Request::ImportClipman { remove } => self.import_clipman(client, remove),
            Request::Thumbnail { id } => {
                let thumbnail =
                    self.thumbnails.as_ref().map_or(Thumb::Absent, |t| t.get(EntryId::new(id)));
                respond(self, Response::Thumbnail { id, thumbnail });
            }
            Request::Input
            | Request::Switch { .. }
            | Request::SwitchHome
            | Request::Link
            | Request::Peers
            | Request::Pair { .. }
            | Request::PairAnswer { .. }
            | Request::Unpair { .. } => {
                self.link_request(client, request);
            }
            // Answered by the control module.
            Request::Hello { .. } | Request::Subscribe => {}
        }
        Ok(())
    }

    fn import_clipman(&mut self, client: ClientId, remove: bool) {
        let response = match import::import(&mut self.tracker, &self.clipman_dir, self.per_target) {
            Ok(done) => {
                log(&format!("clipman import: {} added, {} skipped", done.added, done.skipped));
                if done.added > 0 {
                    self.save();
                }
                if remove && import::remove(&self.clipman_dir).is_err() {
                    Response::Error(Error::ImportFailed)
                } else {
                    Response::Imported { added: done.added, skipped: done.skipped }
                }
            }
            Err(_) => Response::Error(Error::ImportFailed),
        };
        self.clients.respond(client, &response);
    }

    /// Input sharing's requests (docs/input.md, "In the daemon").
    fn input_request(&mut self, client: ClientId, request: Request) {
        use crate::input::State;
        use jackstay_proto::control::{InputState, InputStatus};
        let peers = self.link.as_ref().map(Link::peers).unwrap_or_default();
        let name = |key: jackstay_net::PublicKey| {
            let named = peers.iter().find(|(k, _)| *k == key).map(|(_, n)| n.clone());
            truncate(&printable(&named.unwrap_or_else(|| key.fingerprint())), 64)
        };
        let primary =
            matches!(self.input.state(), State::Here { .. } | State::On { .. } | State::Waiting(_));
        let response = match request {
            Request::Input => {
                let (state, peer, detail) = match self.input.state() {
                    State::Off(why) => (InputState::Off, String::new(), why),
                    State::Waiting(why) => (InputState::Waiting, String::new(), why),
                    State::Here { unanswered: None } => {
                        (InputState::Here, String::new(), String::new())
                    }
                    State::Here { unanswered: Some(peer) } => {
                        let why = format!("{} did not take the last crossing", name(peer));
                        (InputState::Here, String::new(), why)
                    }
                    State::On { peer, locked } => {
                        let state = if locked { InputState::Locked } else { InputState::On };
                        (state, name(peer), String::new())
                    }
                    State::Secondary { primary, driven } => {
                        let state = if driven { InputState::Driven } else { InputState::Secondary };
                        (state, name(primary), String::new())
                    }
                };
                let detail = truncate(&printable(&detail), 128);
                Response::Input(InputStatus { state, peer, detail })
            }
            _ if !primary => Response::Error(Error::InputOff),
            Request::Switch { peer } => {
                let found = self.link.as_ref().map_or(Err(Error::NoSuchPeer), |l| l.find(&peer));
                match found {
                    Ok(key) if self.input.neighbours().contains(&key) => {
                        self.input.tell(crate::input::Command::Switch(Some(key)));
                        Response::Ok
                    }
                    Ok(_) => Response::Error(Error::NotNeighbour),
                    Err(e) => Response::Error(e),
                }
            }
            _ => {
                self.input.tell(crate::input::Command::Switch(None));
                Response::Ok
            }
        };
        self.clients.respond(client, &response);
    }

    /// The link's requests (docs/link.md, "In the daemon").
    fn link_request(&mut self, client: ClientId, request: Request) {
        match request {
            Request::Input | Request::Switch { .. } | Request::SwitchHome => {
                self.input_request(client, request);
            }
            Request::Link | Request::Peers => {
                let status = self.link.as_ref().map_or_else(
                    || jackstay_proto::control::LinkStatus {
                        off: Some("the link could not be set up".to_owned()),
                        ..Default::default()
                    },
                    Link::link_status,
                );
                // A version 3 client's reply leaves out who dialed.
                let reply = if request == Request::Link {
                    Response::Link(status)
                } else {
                    Response::Peers(status)
                };
                self.clients.respond(client, &reply);
            }
            Request::Pair { host } => self.pair(client, &host),
            Request::PairAnswer { yes } => {
                match (self.pairing, self.link.as_ref().and_then(Link::handle)) {
                    (Some(Flow::Client(c)), Some(handle)) if c == client => {
                        handle.confirm(yes);
                        self.clients.respond(client, &Response::Ok);
                    }
                    _ => self.clients.respond(client, &Response::Error(Error::NotPairing)),
                }
            }
            Request::Unpair { peer } => {
                let Some(link) = self.link.as_ref().filter(|l| l.handle().is_some()) else {
                    self.clients.respond(client, &Response::Error(Error::LinkOff));
                    return;
                };
                match link.find(&peer) {
                    Err(e) => self.clients.respond(client, &Response::Error(e)),
                    Ok(key) => {
                        if let Some(handle) = link.handle() {
                            handle.unpair(key);
                        }
                        self.unpairing.push_back(client);
                        self.clients.defer(client);
                    }
                }
            }
            _ => {}
        }
    }

    /// Starts pairing for `client`, which then gets its events.
    fn pair(&mut self, client: ClientId, host: &str) {
        let failed = |reason| Response::PairFailed { reason, detail: String::new() };
        let Some(handle) = self.link.as_ref().and_then(Link::handle) else {
            self.clients.respond(client, &failed(PairReason::LinkOff));
            return;
        };
        if self.pairing.is_some() {
            self.clients.respond(client, &failed(PairReason::Busy));
            return;
        }
        let Some(address) = jackstay_net::Address::parse(host) else {
            self.clients.respond(client, &failed(PairReason::BadAddress));
            return;
        };
        handle.pair(address);
        self.pairing = Some(Flow::Client(client));
        log("pairing started");
        self.clients.respond(client, &Response::Ok);
    }

    /// What the link thread said: status is kept; pairing's events go to the pairing client.
    fn link_events(&mut self) {
        let Some(link) = &mut self.link else { return };
        let events = link.receive();
        // The input thread resolves its neighbours against the peers, whenever they change.
        let peers = link.peers();
        if self.peers_told.as_ref() != Some(&peers) {
            self.input.tell(crate::input::Command::Peers(peers.clone()));
            self.peers_told = Some(peers);
        }
        for event in events {
            match event {
                jackstay_net::Event::PairCode { name, fingerprint, code } => {
                    if let Some(Flow::Client(c)) = self.pairing {
                        let name = truncate(&printable(&name), 64);
                        self.clients.respond(c, &Response::PairCode { name, fingerprint, code });
                    }
                }
                jackstay_net::Event::Paired { name, fingerprint } => {
                    log(&format!("paired with {fingerprint}"));
                    if let Some(Flow::Client(c)) = self.pairing.take() {
                        let name = truncate(&printable(&name), 64);
                        self.clients.respond(c, &Response::Paired { name, fingerprint });
                    }
                }
                jackstay_net::Event::PairFailed(f) => {
                    let (reason, detail) = link::pair_reason(&f);
                    log(&format!("pairing ended: {}", reason.explain()));
                    if let Some(Flow::Client(c)) = self.pairing.take() {
                        self.clients.respond(c, &Response::PairFailed { reason, detail });
                    }
                }
                jackstay_net::Event::Unpaired { fingerprint, saved } => {
                    let response = match saved {
                        Ok(()) => {
                            log(&format!("unpaired {fingerprint}"));
                            Response::Ok
                        }
                        Err(jackstay_net::UnpairError::NotPaired) => {
                            Response::Error(Error::NoSuchPeer)
                        }
                        Err(jackstay_net::UnpairError::Save(_)) => {
                            log(&format!("unpairing {fingerprint}: the peers file was not saved"));
                            Response::Error(Error::SaveFailed)
                        }
                    };
                    if let Some(c) = self.unpairing.pop_front() {
                        self.clients.resolve(c, &response);
                    }
                }
                jackstay_net::Event::Status(_) => {}
            }
        }
    }

    /// Ends pairing if the client that started it has gone.
    fn pairing_client_gone(&mut self) {
        if let Some(Flow::Client(c)) = self.pairing
            && !self.clients.contains(c)
        {
            self.pairing = Some(Flow::Cancelling);
            if let Some(handle) = self.link.as_ref().and_then(Link::handle) {
                handle.cancel_pairing();
            }
            log("pairing cancelled: its client left");
        }
    }

    fn store_replies(&mut self) -> Result<(), Exit> {
        let mut drained = [0u8; 64];
        if let Some(store) = &self.store {
            let _ = (&store.wake).read(&mut drained);
        }
        loop {
            let Some(store) = &self.store else { return Ok(()) };
            let reply = match store.replies.try_recv() {
                Ok(reply) => reply,
                Err(std::sync::mpsc::TryRecvError::Empty) => return Ok(()),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return Err(Exit::StoreGone),
            };
            match reply {
                Reply::Done { ack, result } => {
                    if result.is_err() {
                        self.store_failing = true;
                    }
                    if let Some((client, failure)) = self.waiting.remove(&ack) {
                        let response = match (result, failure) {
                            (Err(_), _) => Response::Error(Error::StoreFailed),
                            (Ok(()), Some(e)) => Response::Error(e),
                            (Ok(()), None) => Response::Ok,
                        };
                        self.clients.resolve(client, &response);
                    }
                    if let Err(e) = result {
                        log(&format!("store: {e}; retrying"));
                    }
                }
                Reply::Failed(e) => {
                    self.store_failing = true;
                    log(&format!("store: {e}; retrying"));
                }
                Reply::Recovered => {
                    self.store_failing = false;
                    log("store: saved again");
                }
                Reply::Corrupt(ids) => {
                    log(&format!("store: {} entries changed on disk; dropped", ids.len()));
                    let mut clear = false;
                    for id in ids {
                        clear |= self.tracker.delete(id) == Some(true);
                    }
                    if clear {
                        self.clear_clipboard()?;
                    }
                    self.save();
                }
            }
        }
    }

    /// Runs until something ends the session.
    pub fn run(&mut self) -> Exit {
        self.notifier.ready();
        loop {
            if let Err(exit) = self.turn() {
                self.notifier.stopping();
                return exit;
            }
            self.notifier.feed();
        }
    }

    /// Waits up to `sleep` for any descriptor, and says which fired.
    fn poll(&self, sleep: Duration) -> Vec<(Slot, bool)> {
        let mut slots = vec![Slot::X, Slot::Signals];
        let mut fds: Vec<(BorrowedFd<'_>, PollFlags)> =
            vec![(self.clipboard.fd(), PollFlags::IN), (self.signals.as_fd(), PollFlags::IN)];
        if let Some(listener) = self.clients.listener_fd() {
            slots.push(Slot::Listener);
            fds.push((listener, PollFlags::IN));
        }
        if let Some(store) = &self.store {
            slots.push(Slot::Store);
            fds.push((store.wake.as_fd(), PollFlags::IN));
        }
        if let Some(t) = &self.thumbnails {
            slots.push(Slot::Thumbnails);
            fds.push((t.wake.as_fd(), PollFlags::IN));
        }
        if let Some(l) = &self.link {
            slots.push(Slot::Link);
            fds.push((l.wake.as_fd(), PollFlags::IN));
        }
        for (id, fd, reading, writing) in self.clients.client_fds() {
            let mut flags = PollFlags::empty();
            if reading {
                flags |= PollFlags::IN;
            }
            if writing {
                flags |= PollFlags::OUT;
            }
            slots.push(Slot::Client(id));
            fds.push((fd, flags));
        }
        let mut polled: Vec<PollFd<'_>> =
            fds.iter().map(|(fd, flags)| PollFd::new(fd, *flags)).collect();
        let timeout = Timespec::try_from(sleep).ok();
        let _ = poll(&mut polled, timeout.as_ref());
        let fired: Vec<(Slot, bool)> =
            slots.iter().zip(&polled).map(|(slot, p)| (*slot, !p.revents().is_empty())).collect();
        drop(polled);
        drop(fds);
        fired
    }

    fn turn(&mut self) -> Result<(), Exit> {
        self.clients.begin_turn();
        let ready = self.clients.ready();
        let sleep = if ready.is_empty() {
            self.clipboard.timeout().map_or(MAX_SLEEP, |t| t.min(MAX_SLEEP))
        } else {
            Duration::ZERO
        };
        let fired = self.poll(sleep);

        let reports = self.clipboard.process().map_err(Exit::XGone)?;
        if self.reports(&reports)? {
            self.save();
        }
        let mut readable = Vec::new();
        for (slot, fired) in fired {
            match slot {
                Slot::Signals if fired => return Err(Exit::Stopped),
                Slot::Listener if fired => {
                    for why in self.clients.accept() {
                        log(&format!("control client refused: {why:?}"));
                    }
                }
                Slot::Store if fired => self.store_replies()?,
                Slot::Thumbnails if fired => {
                    if let Some(t) = &mut self.thumbnails {
                        t.receive();
                    }
                }
                Slot::Link if fired => self.link_events(),
                Slot::Client(id) if fired => readable.push(id),
                Slot::X
                | Slot::Listener
                | Slot::Store
                | Slot::Signals
                | Slot::Thumbnails
                | Slot::Link
                | Slot::Client(_) => {}
            }
        }
        for &id in &readable {
            let _ = self.clients.read(id);
        }
        let mut serve: Vec<ClientId> = readable;
        serve.extend(ready);
        serve.sort_unstable();
        serve.dedup();
        for id in serve {
            loop {
                match self.clients.next(id) {
                    Ok(Some(request)) => self.request(id, request)?,
                    Ok(None) => break,
                    Err(why) => {
                        log(&format!("control client dropped: {why:?}"));
                        break;
                    }
                }
            }
        }
        for why in self.clients.flush() {
            log(&format!("control client dropped: {why:?}"));
        }
        self.pairing_client_gone();
        if let Some(l) = &mut self.link {
            l.check().map_err(Exit::LinkGone)?;
        }
        self.input.check().map_err(Exit::InputGone)?;
        if self.last_clipman_check.elapsed() >= CLIPMAN_CHECK_EVERY {
            self.last_clipman_check = Instant::now();
            if (self.clipman_check)() {
                return Err(Exit::Clipman);
            }
        }
        Ok(())
    }
}
