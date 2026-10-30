//! The dead man's switch (docs/input.md, "Primary", "Watchdog"): a thread with no X and no locks
//! that shuts the input connection's socket when the input thread holds the grab and has stopped
//! checking in. The X server then releases every grab and barrier of the closed client.

use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How often the watchdog looks.
const TICK: Duration = Duration::from_millis(100);

#[derive(Debug)]
struct State {
    started: Instant,
    /// Milliseconds after `started` of the last check-in.
    beat: AtomicU64,
    grabbing: AtomicBool,
    fired: AtomicBool,
    stop: AtomicBool,
}

impl State {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

#[derive(Debug)]
pub struct Watchdog {
    state: Arc<State>,
    thread: Option<JoinHandle<()>>,
}

impl Watchdog {
    /// Watches the X connection whose socket `socket` duplicates: if the input thread holds the
    /// grab and has not checked in for `limit`, the socket is shut down.
    ///
    /// # Errors
    /// The thread could not be started.
    pub fn start(socket: OwnedFd, limit: Duration) -> std::io::Result<Watchdog> {
        let state = Arc::new(State {
            started: Instant::now(),
            beat: AtomicU64::new(0),
            grabbing: AtomicBool::new(false),
            fired: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });
        let watched = Arc::clone(&state);
        let limit_ms = u64::try_from(limit.as_millis()).unwrap_or(u64::MAX);
        let thread =
            std::thread::Builder::new().name("input-watchdog".into()).spawn(move || {
                let socket = std::os::unix::net::UnixStream::from(socket);
                while !watched.stop.load(Ordering::Acquire) {
                    std::thread::sleep(TICK);
                    // Grabbing first: `beat` stores the time before the flag, so a flag seen
                    // set comes with the beat stored with it, never an older one.
                    let grabbing = watched.grabbing.load(Ordering::Acquire);
                    let silent =
                        watched.now_ms().saturating_sub(watched.beat.load(Ordering::Acquire));
                    if grabbing && silent >= limit_ms {
                        let _ = socket.shutdown(std::net::Shutdown::Both);
                        watched.fired.store(true, Ordering::Release);
                        return;
                    }
                }
            })?;
        Ok(Watchdog { state, thread: Some(thread) })
    }

    /// The input thread is alive; whether it holds the grab.
    pub fn beat(&self, grabbing: bool) {
        self.state.beat.store(self.state.now_ms(), Ordering::Release);
        self.state.grabbing.store(grabbing, Ordering::Release);
    }

    /// Whether the watchdog shut the connection.
    #[must_use]
    pub fn fired(&self) -> bool {
        self.state.fired.load(Ordering::Acquire)
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
