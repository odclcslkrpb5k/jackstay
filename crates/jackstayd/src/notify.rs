//! systemd's notification protocol (`sd_notify`): `READY=1` once serving, `WATCHDOG=1` from every
//! turn of the loop, `STOPPING=1` on the way out (docs/daemon.md, "Liveness"). Without
//! `$NOTIFY_SOCKET` (not started by systemd) every call does nothing.

use std::os::linux::net::SocketAddrExt as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::time::{Duration, Instant};

/// How often the watchdog is fed at most; systemd's `WatchdogSec=30` is far above it.
const FEED_EVERY: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub struct Notifier {
    socket: Option<(UnixDatagram, SocketAddr)>,
    last_feed: Option<Instant>,
}

impl Notifier {
    /// From `$NOTIFY_SOCKET`: a path, or an abstract name written with a leading `@`.
    #[must_use]
    pub fn from_env() -> Self {
        let socket = std::env::var_os("NOTIFY_SOCKET").and_then(|path| {
            let path = path.into_encoded_bytes();
            let addr = match path.strip_prefix(b"@") {
                Some(name) => SocketAddr::from_abstract_name(name).ok()?,
                None => SocketAddr::from_pathname(std::ffi::OsStr::from_bytes(&path)).ok()?,
            };
            Some((UnixDatagram::unbound().ok()?, addr))
        });
        Notifier { socket, last_feed: None }
    }

    fn send(&self, message: &str) {
        if let Some((socket, addr)) = &self.socket {
            let _ = socket.send_to_addr(message.as_bytes(), addr);
        }
    }

    pub fn ready(&self) {
        self.send("READY=1");
    }

    pub fn stopping(&self) {
        self.send("STOPPING=1");
    }

    /// Feeds the watchdog, at most once a second.
    pub fn feed(&mut self) {
        if self.last_feed.is_none_or(|t| t.elapsed() >= FEED_EVERY) {
            self.send("WATCHDOG=1");
            self.last_feed = Some(Instant::now());
        }
    }
}
