//! The control socket's server side (docs/daemon.md, "Control socket"): accepting clients of this
//! user only, reading their requests and writing responses without ever blocking the clipboard
//! thread, and bounding what any client can cost it. What a request does is the session's.
//!
//! The bounds: at most [`MAX_CLIENTS`]; a client is read only while it has little buffered
//! ([`MAX_INPUT`]) and has taken most of its responses ([`HIGH_WATER`]); at most
//! [`REQUESTS_PER_TURN`] of its requests run per turn; one whose unread output passes
//! [`MAX_OUTPUT`] is dropped; one that has not said `Hello` within [`HELLO_WITHIN`] is dropped.

use std::collections::BTreeMap;
use std::io::{self, Read as _, Write as _};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant};

use jackstay_proto::control::{self, Error, OLDEST_VERSION, Request, Response, VERSION};

/// The most clients connected at once.
pub const MAX_CLIENTS: usize = 16;
/// The most output queued for one client; one that falls this far behind is disconnected.
pub const MAX_OUTPUT: usize = 4 << 20;
/// Output queued beyond which a client's further requests wait until it reads.
pub const HIGH_WATER: usize = 1 << 20;
/// Input buffered beyond which a client is not read until its requests are handled.
pub const MAX_INPUT: usize = 64 << 10;
/// The most requests from one client handled in one turn of the loop.
pub const REQUESTS_PER_TURN: usize = 64;
/// How long a new client has to say `Hello`.
pub const HELLO_WITHIN: Duration = Duration::from_secs(10);
/// How long accepting pauses after the listener fails (out of descriptors, say), so the loop does
/// not spin on a connection it cannot take.
const ACCEPT_PAUSE: Duration = Duration::from_secs(1);

/// One connected client, by a number never reused while the daemon runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClientId(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Connected; its first request must be `Hello`.
    New,
    Greeted,
    /// Close once the output is written: the client broke the protocol, or finished sending.
    Closing,
}

#[derive(Debug)]
struct Client {
    stream: UnixStream,
    input: Vec<u8>,
    output: Vec<u8>,
    phase: Phase,
    subscribed: bool,
    /// Waiting for a response the session will give later (a clear the store must confirm);
    /// its further requests wait, so responses stay in order.
    busy: bool,
    /// It closed its end for writing: what it sent is still handled, then it is closed.
    eof: bool,
    connected_at: Instant,
    handled_this_turn: usize,
}

/// Why a client was disconnected, for the journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dropped {
    OtherUser,
    TooManyClients,
    /// The socket could not be set up.
    Failed,
    Malformed,
    TooSlow,
    NoHello,
    Closed,
}

#[derive(Debug)]
pub struct Clients {
    listener: UnixListener,
    connected: BTreeMap<ClientId, Client>,
    next: u64,
    uid: rustix::process::Uid,
    paused_until: Option<Instant>,
}

impl Clients {
    /// Listens on `path`, which must not exist (a stale socket is removed by the caller, under the
    /// instance lock).
    ///
    /// # Errors
    /// The socket cannot be bound.
    pub fn bind(path: &Path) -> io::Result<Self> {
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        Ok(Clients {
            listener,
            connected: BTreeMap::new(),
            next: 0,
            uid: rustix::process::getuid(),
            paused_until: None,
        })
    }

    /// The listener, to poll for new clients, unless accepting is paused.
    #[must_use]
    pub fn listener_fd(&self) -> Option<BorrowedFd<'_>> {
        let paused = self.paused_until.is_some_and(|t| Instant::now() < t);
        (!paused).then(|| self.listener.as_fd())
    }

    /// Each client's descriptor and what to poll it for: reading while it can take more, writing
    /// while it has output waiting. Hang-ups are reported whatever is asked.
    pub fn client_fds(&self) -> impl Iterator<Item = (ClientId, BorrowedFd<'_>, bool, bool)> {
        self.connected
            .iter()
            .map(|(id, c)| (*id, c.stream.as_fd(), c.wants_read(), !c.output.is_empty()))
    }

    /// Starts a turn of the loop: each client may have [`REQUESTS_PER_TURN`] handled again.
    pub fn begin_turn(&mut self) {
        for c in self.connected.values_mut() {
            c.handled_this_turn = 0;
        }
    }

    /// Accepts every waiting connection: another user's, or one past [`MAX_CLIENTS`], is closed at
    /// once, before anything is read.
    pub fn accept(&mut self) -> Vec<Dropped> {
        let mut dropped = Vec::new();
        loop {
            let stream = match self.listener.accept() {
                Ok((stream, _)) => stream,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted
                    ) =>
                {
                    continue;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return dropped,
                Err(_) => {
                    self.paused_until = Some(Instant::now() + ACCEPT_PAUSE);
                    dropped.push(Dropped::Failed);
                    return dropped;
                }
            };
            let same_user = rustix::net::sockopt::socket_peercred(&stream)
                .is_ok_and(|cred| cred.uid == self.uid);
            if !same_user {
                dropped.push(Dropped::OtherUser);
                continue;
            }
            if self.connected.len() >= MAX_CLIENTS {
                dropped.push(Dropped::TooManyClients);
                continue;
            }
            if stream.set_nonblocking(true).is_err() {
                dropped.push(Dropped::Failed);
                continue;
            }
            let id = ClientId(self.next);
            self.next = self.next.saturating_add(1);
            self.connected.insert(
                id,
                Client {
                    stream,
                    input: Vec::new(),
                    output: Vec::new(),
                    phase: Phase::New,
                    subscribed: false,
                    busy: false,
                    eof: false,
                    connected_at: Instant::now(),
                    handled_this_turn: 0,
                },
            );
        }
    }

    /// Reads what `id` sent, while it can take more.
    ///
    /// # Errors
    /// Why the client was dropped: its connection failed.
    pub fn read(&mut self, id: ClientId) -> Result<(), Dropped> {
        let Some(c) = self.connected.get_mut(&id) else { return Ok(()) };
        let result = c.read();
        if result.is_err() {
            self.connected.remove(&id);
        }
        result
    }

    /// The next request from `id` that the session must handle, if one is whole and the client may
    /// have it handled now: `Hello`, `Subscribe` and requests out of sequence are answered here.
    /// One at a time, so a request the session defers holds back the ones after it.
    ///
    /// # Errors
    /// Why the client was dropped: a malformed frame.
    pub fn next(&mut self, id: ClientId) -> Result<Option<Request>, Dropped> {
        let Some(c) = self.connected.get_mut(&id) else { return Ok(None) };
        let result = c.next();
        if result.is_err() {
            self.connected.remove(&id);
        }
        result
    }

    /// Queues `response` for `id`.
    pub fn respond(&mut self, id: ClientId, response: &Response) {
        if let Some(c) = self.connected.get_mut(&id) {
            queue(c, response);
        }
    }

    /// Marks `id` as waiting for a response the session gives later with [`Clients::resolve`].
    pub fn defer(&mut self, id: ClientId) {
        if let Some(c) = self.connected.get_mut(&id) {
            c.busy = true;
        }
    }

    /// Gives a deferred response, and lets the client's next requests through.
    pub fn resolve(&mut self, id: ClientId, response: &Response) {
        if let Some(c) = self.connected.get_mut(&id) {
            c.busy = false;
            queue(c, response);
            if c.eof && c.input.is_empty() {
                c.phase = Phase::Closing;
            }
        }
    }

    /// Tells every subscribed client that the history changed.
    pub fn changed(&mut self) {
        for c in self.connected.values_mut().filter(|c| c.subscribed && c.phase != Phase::Closing) {
            queue(c, &Response::Changed);
        }
    }

    /// Clients with requests already buffered that may be handled now, to serve again without
    /// waiting for their socket.
    #[must_use]
    pub fn ready(&self) -> Vec<ClientId> {
        self.connected
            .iter()
            .filter(|(_, c)| c.may_handle() && c.has_work())
            .map(|(id, _)| *id)
            .collect()
    }

    /// Writes what each client can take now; drops one whose output passed [`MAX_OUTPUT`], one
    /// closing with nothing left to write, and one that never said `Hello`.
    pub fn flush(&mut self) -> Vec<Dropped> {
        let mut gone = Vec::new();
        for (id, c) in &mut self.connected {
            while !c.output.is_empty() {
                match c.stream.write(&c.output) {
                    Ok(0) => break,
                    Ok(n) => {
                        c.output.drain(..n.min(c.output.len()));
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        gone.push((*id, Dropped::Closed));
                        break;
                    }
                }
            }
            if c.output.len() > MAX_OUTPUT {
                gone.push((*id, Dropped::TooSlow));
            } else if c.phase == Phase::Closing && c.output.is_empty() {
                gone.push((*id, Dropped::Closed));
            } else if c.phase == Phase::New && c.connected_at.elapsed() >= HELLO_WITHIN {
                gone.push((*id, Dropped::NoHello));
            }
        }
        let mut dropped = Vec::new();
        for (id, why) in gone {
            if self.connected.remove(&id).is_some() && why != Dropped::Closed {
                dropped.push(why);
            }
        }
        dropped
    }

    /// Whether `id` is still connected.
    #[must_use]
    pub fn contains(&self, id: ClientId) -> bool {
        self.connected.contains_key(&id)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.connected.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.connected.is_empty()
    }
}

impl Client {
    /// Whether its requests may be handled now.
    fn may_handle(&self) -> bool {
        !self.busy
            && self.phase != Phase::Closing
            && self.output.len() < HIGH_WATER
            && self.handled_this_turn < REQUESTS_PER_TURN
    }

    fn wants_read(&self) -> bool {
        !self.busy
            && self.phase != Phase::Closing
            && !self.eof
            && self.input.len() < MAX_INPUT
            && self.output.len() < HIGH_WATER
    }

    /// Whether a whole frame (or a malformed one, to be refused) is buffered, or it has finished
    /// sending: then there is something to do without waiting for its socket. A partial frame is
    /// not, or a client that stalls mid-frame would keep the loop from sleeping.
    fn has_work(&self) -> bool {
        self.eof || !matches!(control::decode_request(&self.input), Ok(None))
    }

    fn read(&mut self) -> Result<(), Dropped> {
        let mut buf = [0u8; 4096];
        while self.wants_read() {
            match self.stream.read(&mut buf) {
                Ok(0) => self.eof = true,
                Ok(n) => self.input.extend_from_slice(buf.get(..n).unwrap_or_default()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => return Err(Dropped::Closed),
            }
        }
        if self.eof && self.input.is_empty() && !self.busy {
            self.phase = Phase::Closing;
        }
        Ok(())
    }

    fn next(&mut self) -> Result<Option<Request>, Dropped> {
        while self.may_handle() {
            let (request, used) = match control::decode_request(&self.input) {
                Ok(Some(decoded)) => decoded,
                Ok(None) => {
                    if self.eof {
                        self.phase = Phase::Closing;
                    }
                    return Ok(None);
                }
                Err(_) => return Err(Dropped::Malformed),
            };
            self.input.drain(..used);
            self.handled_this_turn = self.handled_this_turn.saturating_add(1);
            let greeted = self.phase == Phase::Greeted;
            match (greeted, request) {
                (false, Request::Hello { version })
                    if (OLDEST_VERSION..=VERSION).contains(&version) =>
                {
                    self.phase = Phase::Greeted;
                    // Its own version back: the newer requests it does not know, it never sends.
                    queue(self, &Response::Welcome { version });
                }
                (false, Request::Hello { .. }) => {
                    queue(self, &Response::Error(Error::UnsupportedVersion));
                    self.phase = Phase::Closing;
                }
                (true, Request::Hello { .. }) | (false, _) => {
                    queue(self, &Response::Error(Error::BadSequence));
                    self.phase = Phase::Closing;
                }
                (true, Request::Subscribe) => {
                    self.subscribed = true;
                    queue(self, &Response::Ok);
                }
                (true, request) => return Ok(Some(request)),
            }
        }
        Ok(None)
    }
}

fn queue(c: &mut Client, response: &Response) {
    // A response that breaks a limit is a daemon bug; the client gets an error rather than nothing.
    if control::encode_response(response, &mut c.output).is_err() {
        let _ = control::encode_response(&Response::Error(Error::Internal), &mut c.output);
    }
}
