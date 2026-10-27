//! Jackstay's clipboard on a real X connection: the selection engine (`jackstay-selection`)
//! driven by X events, with x11rb's pure-Rust connection underneath. No C in this path.
//!
//! The daemon owns the loop: it polls [`Clipboard::fd`] (and its own wake-ups) with
//! [`Clipboard::timeout`], then calls [`Clipboard::process`].

mod decode;
mod port;

use std::os::fd::{AsFd, BorrowedFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use jackstay_clip::Content;
use jackstay_selection::{Config, Engine, Millis, Report, Window};
use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::errors::{ConnectError, ConnectionError, ReplyError, ReplyOrIdError};
use x11rb::protocol::res::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    self, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, WindowClass,
};
use x11rb::rust_connection::RustConnection;

use crate::port::X11Port;

/// Why the clipboard session cannot run. All end the session; the daemon reports and exits, and
/// systemd decides about a restart (plan §5.6).
#[derive(Debug)]
pub enum Error {
    /// No X server to connect to (`DISPLAY` unset or wrong, or the server is gone).
    Connect(ConnectError),
    /// The connection failed or closed while running: logout, or the server died.
    Connection(ConnectionError),
    /// The server lacks `XFixes` 2 or later, which owner-change notifications need.
    NoXFixes,
    /// An X error while setting up our window and event selection.
    Setup(ReplyError),
    /// The connection has no window ids left to give out.
    Ids,
    /// The display has no such screen.
    NoScreen,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Connect(e) => write!(f, "cannot connect to the X server: {e}"),
            Error::Connection(e) => write!(f, "X connection lost: {e}"),
            Error::NoXFixes => write!(f, "the X server lacks XFixes 2"),
            Error::Setup(e) => write!(f, "X setup failed: {e}"),
            Error::Ids => write!(f, "no X resource ids left"),
            Error::NoScreen => write!(f, "the X display has no such screen"),
        }
    }
}

impl std::error::Error for Error {}

impl From<ConnectionError> for Error {
    fn from(e: ConnectionError) -> Self {
        Error::Connection(e)
    }
}

impl From<ReplyError> for Error {
    fn from(e: ReplyError) -> Self {
        match e {
            ReplyError::ConnectionError(e) => Error::Connection(e),
            ReplyError::X11Error(_) => Error::Setup(e),
        }
    }
}

/// The clipboard: one X connection, our window, and the selection engine.
#[derive(Debug)]
pub struct Clipboard {
    port: X11Port,
    engine: Engine,
    started: Instant,
}

impl Clipboard {
    /// Connects to `display` (or `$DISPLAY`), creates our window, asks for owner-change
    /// notifications on CLIPBOARD, and starts the engine.
    ///
    /// # Errors
    /// [`Error`] if there is no usable X server.
    pub fn connect(display: Option<&str>, config: Config) -> Result<(Self, Vec<Report>), Error> {
        let (conn, screen) = RustConnection::connect(display).map_err(Error::Connect)?;
        let root = conn.setup().roots.get(screen).map(|s| s.root).ok_or(Error::NoScreen)?;
        let version = conn.xfixes_query_version(5, 0)?.reply()?;
        if version.major_version < 2 {
            return Err(Error::NoXFixes);
        }
        let window = conn.generate_id().map_err(|e| match e {
            ReplyOrIdError::ConnectionError(e) => Error::Connection(e),
            ReplyOrIdError::X11Error(e) => Error::Setup(ReplyError::X11Error(e)),
            ReplyOrIdError::IdsExhausted => Error::Ids,
        })?;
        let aux = CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE);
        conn.create_window(0, window, root, 0, 0, 1, 1, 0, WindowClass::INPUT_ONLY, 0, &aux)?
            .check()?;
        // Other clients' attribution of our copies (as for any owner) reads WM_CLASS.
        let class = b"jackstay\0Jackstay\0";
        conn.change_property(
            PropMode::REPLACE,
            window,
            xproto::AtomEnum::WM_CLASS,
            xproto::AtomEnum::STRING,
            8,
            u32::try_from(class.len()).unwrap_or(0),
            class,
        )?;
        let clipboard = conn.intern_atom(false, b"CLIPBOARD")?.reply()?.atom;
        let time_property = conn.intern_atom(false, b"JACKSTAY_TIME")?.reply()?.atom;
        let mask = xfixes::SelectionEventMask::SET_SELECTION_OWNER
            | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
            | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE;
        conn.xfixes_select_selection_input(window, clipboard, mask)?.check()?;
        // X-Resource 1.2 names a client's process (attribution); without it, no request of it is
        // sent, since x11rb fails the connection for an absent extension.
        let resources = conn.extension_information(res::X11_EXTENSION_NAME)?.is_some()
            && conn
                .res_query_version(1, 2)?
                .reply()
                .is_ok_and(|v| (v.server_major, v.server_minor) >= (1, 2));
        let mut port = X11Port::new(conn, root, window, time_property, resources);
        let started = Instant::now();
        let (engine, reports) = Engine::new(&mut port, Window(window), config, Millis(0));
        let mut clipboard = Clipboard { port, engine, started };
        clipboard.finish_turn()?;
        Ok((clipboard, reports))
    }

    /// The X connection's file descriptor, readable when events may be waiting.
    #[must_use]
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.port.conn.stream().as_fd()
    }

    /// How long until [`Clipboard::process`] is needed even if the fd stays quiet: zero if events
    /// are already waiting in memory, `None` if nothing is pending at all.
    #[must_use]
    pub fn timeout(&self) -> Option<Duration> {
        if !self.port.queued.is_empty() {
            return Some(Duration::ZERO);
        }
        let deadline = self.engine.next_deadline()?;
        Some(Duration::from_millis(deadline.0.saturating_sub(self.now().0)))
    }

    /// Handles every event waiting, then every deadline passed.
    ///
    /// # Errors
    /// [`Error::Connection`] if the X connection is gone.
    pub fn process(&mut self) -> Result<Vec<Report>, Error> {
        let mut reports = Vec::new();
        loop {
            let event = match self.port.queued.pop_front() {
                Some(e) => Some(e),
                None => self.port.conn.poll_for_event()?,
            };
            let Some(event) = event else { break };
            if let Some(event) = decode::event(&event) {
                let now = self.now();
                reports.extend(self.engine.handle(&mut self.port, event, now));
            }
            self.check()?;
        }
        let now = self.now();
        if self.engine.next_deadline().is_some_and(|d| d <= now) {
            reports.extend(self.engine.tick(&mut self.port, now));
        }
        self.finish_turn()?;
        Ok(reports)
    }

    /// Makes `content` the clipboard, as when the user re-selects a history entry.
    ///
    /// # Errors
    /// [`Error::Connection`] if the X connection is gone.
    pub fn serve(&mut self, content: Arc<Content>) -> Result<Vec<Report>, Error> {
        let now = self.now();
        let reports = self.engine.serve(&mut self.port, content, now);
        self.finish_turn()?;
        Ok(reports)
    }

    /// Serves `content`, saved from an earlier run, only if no client owns the clipboard; reports
    /// `Claimed` if it did.
    ///
    /// # Errors
    /// [`Error::Connection`] if the X connection is gone.
    pub fn restore(&mut self, content: Arc<Content>) -> Result<Vec<Report>, Error> {
        let now = self.now();
        let reports = self.engine.restore(&mut self.port, content, now);
        self.finish_turn()?;
        Ok(reports)
    }

    /// Clears the clipboard, and PRIMARY.
    ///
    /// # Errors
    /// [`Error::Connection`] if the X connection is gone.
    pub fn clear(&mut self) -> Result<Vec<Report>, Error> {
        let now = self.now();
        let reports = self.engine.clear(&mut self.port, now);
        self.finish_turn()?;
        Ok(reports)
    }

    /// Whether the engine believes it owns CLIPBOARD.
    #[must_use]
    pub fn owns_clipboard(&self) -> bool {
        self.engine.owns_clipboard()
    }

    fn now(&self) -> Millis {
        Millis(u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX))
    }

    fn check(&mut self) -> Result<(), Error> {
        match self.port.broken.take() {
            Some(e) => Err(Error::Connection(e)),
            None => Ok(()),
        }
    }

    /// Ends a turn: reports a broken connection, flushes, and moves every event x11rb already
    /// holds in memory into our queue. Such events leave the socket unreadable, so a poll on
    /// [`Clipboard::fd`] would sleep through them; [`Clipboard::timeout`] is zero while any wait.
    fn finish_turn(&mut self) -> Result<(), Error> {
        self.check()?;
        self.port.conn.flush()?;
        while let Some(event) = self.port.conn.poll_for_event()? {
            self.port.queued.push_back(event);
        }
        Ok(())
    }
}
