//! A private Xvfb per test, and the clipboard session running on a thread beside it.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use jackstay_clip::Content;
use jackstay_selection::{Config, Limits, Report};
use jackstay_x11_clip::Clipboard;
use rustix::event::{PollFd, PollFlags, Timespec, poll};

/// An Xvfb of its own, on a display number unique within this test process. Starting several at
/// once, as parallel tests do, needs two things here: the socket directory must exist before any
/// starts (as a non-root user each tries to create it, and those that lose the race treat
/// `EEXIST` as fatal and never listen), and the pipe `-displayfd` writes to must stay open while
/// the server runs (closing it killed servers with `SIGPIPE`). Each server dies with the test
/// process (`setpriv --pdeathsig`), even if the process is killed.
pub struct Xvfb {
    child: Child,
    pub display: String,
    /// Kept open: see above.
    _ready: BufReader<std::process::ChildStdout>,
}

static NEXT_DISPLAY: AtomicU32 = AtomicU32::new(0);
static SOCKET_DIR: std::sync::Once = std::sync::Once::new();

impl Xvfb {
    pub fn start() -> Self {
        Self::start_with("")
    }

    /// One started with these extra server options (`-extension X-Resource` turns it off).
    pub fn start_with(options: &str) -> Self {
        SOCKET_DIR.call_once(|| {
            let dir = std::path::Path::new("/tmp/.X11-unix");
            let _ = std::fs::create_dir(dir);
            let _ =
                std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o1777));
        });
        let base = 100 + (std::process::id() % 200) * 256;
        let number = base + NEXT_DISPLAY.fetch_add(1, Ordering::Relaxed) % 256;
        // `-displayfd` still tells us when the server is ready to accept connections.
        // -noreset: a server whose last client disconnects resets, and closes connections made
        // meanwhile (xfsettingsd's, under load, after a test's own query: CI, 2026-10-27).
        let script = format!(
            "exec setpriv --pdeathsig KILL Xvfb :{number} -displayfd 3 -noreset -nolisten tcp -screen 0 640x480x24 {options} 3>&1 1>/dev/null 2>/dev/null"
        );
        let mut child = Command::new("sh")
            .args(["-c", &script])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("cannot start Xvfb: {e}"));
        let mut line = String::new();
        let stdout = child.stdout.take().unwrap_or_else(|| panic!("no Xvfb stdout"));
        let mut ready = BufReader::new(stdout);
        let started = ready.read_line(&mut line).is_ok() && line.trim() == number.to_string();
        // Built before the check, so a failed start is still killed when it drops.
        let x = Xvfb { child, display: format!(":{number}"), _ready: ready };
        assert!(started, "Xvfb :{number} did not start: {line:?}");
        x
    }

    /// A program run against this display.
    pub fn command(&self, program: &str) -> Command {
        let mut c = Command::new(program);
        c.env("DISPLAY", &self.display);
        c
    }
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

enum Ask {
    Serve(Arc<Content>),
    Restore(Arc<Content>),
    Clear,
    Owns(Sender<bool>),
    Debug(Sender<String>),
    Timeout(Sender<Option<Duration>>),
    Stop,
}

/// What the session thread tells the test.
pub enum Out {
    /// Connected, subscribed and running.
    Started,
    Report(Report),
    /// The session stopped with this error.
    Ended(String),
}

/// The clipboard session on its own thread, as the daemon will run it.
pub struct Session {
    commands: Sender<Ask>,
    /// Written after each command, so the session's poll wakes at once, as the daemon's will.
    wake: std::os::unix::net::UnixStream,
    reports: Receiver<Out>,
    thread: Option<JoinHandle<()>>,
}

pub fn config() -> Config {
    Config { limits: Limits { per_target: 64 << 20, per_entry: 128 << 20 } }
}

impl Session {
    pub fn start(x: &Xvfb) -> Self {
        Self::start_with(x, config())
    }

    pub fn start_with(x: &Xvfb, config: Config) -> Self {
        let display = x.display.clone();
        let (commands, command_rx) = mpsc::channel();
        let (report_tx, reports) = mpsc::channel();
        let (wake, woken) =
            std::os::unix::net::UnixStream::pair().unwrap_or_else(|e| panic!("wake pipe: {e}"));
        let thread =
            std::thread::spawn(move || run(&display, config, &command_rx, &report_tx, &woken));
        let session = Session { commands, wake, reports, thread: Some(thread) };
        // Owners a test starts afterwards are seen by their notification, never at start-up
        // (whose owner cannot be kept alive: docs/selection.md, "Keep-alive").
        match session.reports.recv_timeout(Duration::from_secs(10)) {
            Ok(Out::Started) => session,
            Ok(Out::Ended(e)) => panic!("session did not start: {e}"),
            Ok(Out::Report(_)) | Err(_) => panic!("session did not start"),
        }
    }

    fn ask(&self, ask: Ask) {
        let _ = self.commands.send(ask);
        let _ = (&self.wake).write_all(b"w");
    }

    pub fn serve(&self, content: Arc<Content>) {
        self.ask(Ask::Serve(content));
    }

    pub fn restore(&self, content: Arc<Content>) {
        self.ask(Ask::Restore(content));
    }

    pub fn clear(&self) {
        self.ask(Ask::Clear);
    }

    /// Whether the session believes it owns CLIPBOARD.
    pub fn owns_clipboard(&self) -> bool {
        let (tx, rx) = mpsc::channel();
        self.ask(Ask::Owns(tx));
        rx.recv_timeout(Duration::from_secs(5)).unwrap_or_else(|e| panic!("owns: {e}"))
    }

    /// The session's `Clipboard`, as `{:?}` prints it.
    pub fn debug(&self) -> String {
        let (tx, rx) = mpsc::channel();
        self.ask(Ask::Debug(tx));
        rx.recv_timeout(Duration::from_secs(5)).unwrap_or_else(|e| panic!("debug: {e}"))
    }

    /// How long until the session needs attention without events.
    pub fn timeout(&self) -> Option<Duration> {
        let (tx, rx) = mpsc::channel();
        self.ask(Ask::Timeout(tx));
        rx.recv_timeout(Duration::from_secs(5)).unwrap_or_else(|e| panic!("timeout: {e}"))
    }

    /// Waits up to `within` for a report that `want` accepts, and returns it.
    pub fn wait_for(
        &self,
        within: Duration,
        mut want: impl FnMut(&Report) -> bool,
    ) -> Option<Report> {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.reports.recv_timeout(left) {
                Ok(Out::Report(r)) if want(&r) => return Some(r),
                Ok(Out::Report(_) | Out::Started) => {}
                Ok(Out::Ended(e)) => panic!("session ended: {e}"),
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    /// Waits up to `within` for the session to end, and returns why.
    pub fn ended(&self, within: Duration) -> Option<String> {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.reports.recv_timeout(left) {
                Ok(Out::Ended(e)) => return Some(e),
                Ok(Out::Report(_) | Out::Started) => {}
                Err(_) => return None,
            }
        }
    }

    /// The next completed capture's content and source.
    pub fn next_capture_with_source(&self) -> (Arc<Content>, Option<String>) {
        match self.wait_for(Duration::from_secs(10), |r| {
            matches!(r, Report::Captured { complete: true, .. })
        }) {
            Some(Report::Captured { content, source, .. }) => (content, source),
            other => panic!("no completed capture: {other:?}"),
        }
    }

    /// The final content of the next capture to complete.
    pub fn next_capture(&self) -> Arc<Content> {
        match self.wait_for(Duration::from_secs(10), |r| {
            matches!(r, Report::Captured { complete: true, .. })
        }) {
            Some(Report::Captured { content, .. }) => content,
            other => panic!("no completed capture: {other:?}"),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.ask(Ask::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(
    display: &str,
    config: Config,
    commands: &Receiver<Ask>,
    out: &Sender<Out>,
    woken: &std::os::unix::net::UnixStream,
) {
    let (mut clipboard, first) = match Clipboard::connect(Some(display), config) {
        Ok(c) => c,
        Err(e) => {
            let _ = out.send(Out::Ended(e.to_string()));
            return;
        }
    };
    let _ = out.send(Out::Started);
    for r in first {
        let _ = out.send(Out::Report(r));
    }
    let ended = |e: jackstay_x11_clip::Error| {
        let _ = out.send(Out::Ended(e.to_string()));
    };
    let _ = woken.set_nonblocking(true);
    loop {
        // As the daemon will: wait for X events or a command, at most until the engine's deadline.
        let timeout = clipboard.timeout().and_then(|t| Timespec::try_from(t).ok());
        {
            let fd = clipboard.fd();
            let mut fds = [PollFd::new(&fd, PollFlags::IN), PollFd::new(woken, PollFlags::IN)];
            let _ = poll(&mut fds, timeout.as_ref());
        }
        let _ = std::io::Read::read(&mut &*woken, &mut [0u8; 64]);
        let mut reports = match clipboard.process() {
            Ok(r) => r,
            Err(e) => return ended(e),
        };
        loop {
            let done = match commands.try_recv() {
                Ok(Ask::Serve(c)) => clipboard.serve(c),
                Ok(Ask::Restore(c)) => clipboard.restore(c),
                Ok(Ask::Clear) => clipboard.clear(),
                Ok(Ask::Owns(reply)) => {
                    let _ = reply.send(clipboard.owns_clipboard());
                    Ok(Vec::new())
                }
                Ok(Ask::Debug(reply)) => {
                    let _ = reply.send(format!("{clipboard:?}"));
                    Ok(Vec::new())
                }
                Ok(Ask::Timeout(reply)) => {
                    let _ = reply.send(clipboard.timeout());
                    Ok(Vec::new())
                }
                Ok(Ask::Stop) | Err(mpsc::TryRecvError::Disconnected) => return,
                Err(mpsc::TryRecvError::Empty) => break,
            };
            match done {
                Ok(r) => reports.extend(r),
                Err(e) => return ended(e),
            }
        }
        for r in reports {
            let _ = out.send(Out::Report(r));
        }
    }
}

/// Puts `data` on the clipboard with xclip, which keeps running as its owner until killed.
pub fn xclip_copy(x: &Xvfb, target: &str, data: &[u8]) -> Child {
    xclip_copy_to(x, "clipboard", target, data)
}

/// Puts `data` on `selection` (`clipboard` or `primary`) with xclip.
pub fn xclip_copy_to(x: &Xvfb, selection: &str, target: &str, data: &[u8]) -> Child {
    let mut child = x
        .command("xclip")
        .args(["-quiet", "-selection", selection, "-target", target, "-i"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("xclip: {e}"));
    let mut stdin = child.stdin.take().unwrap_or_else(|| panic!("no xclip stdin"));
    stdin.write_all(data).unwrap_or_else(|e| panic!("xclip stdin: {e}"));
    drop(stdin);
    child
}

/// Reads the clipboard with xclip, as an application pasting would.
pub fn xclip_paste(x: &Xvfb, target: &str) -> Option<Vec<u8>> {
    xclip_paste_from(x, "clipboard", target)
}

/// Reads `selection` (`clipboard` or `primary`) with xclip.
pub fn xclip_paste_from(x: &Xvfb, selection: &str, target: &str) -> Option<Vec<u8>> {
    let out = x
        .command("timeout")
        .args(["10", "xclip", "-selection", selection, "-target", target, "-o"])
        .stderr(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("xclip -o: {e}"));
    out.status.success().then_some(out.stdout)
}

pub fn xsel_paste(x: &Xvfb) -> Option<Vec<u8>> {
    let out = x
        .command("timeout")
        .args(["10", "xsel", "--clipboard", "--output"])
        .stderr(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("xsel: {e}"));
    out.status.success().then_some(out.stdout)
}

/// Waits up to 5 s until nothing owns CLIPBOARD, as after its owner's connection closes.
pub fn wait_unowned(x: &Xvfb) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while xclip_paste(x, "TARGETS").is_some() {
        assert!(Instant::now() < deadline, "CLIPBOARD still owned");
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub fn kill(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}
