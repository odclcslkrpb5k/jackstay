//! What the end-to-end tests share: an Xvfb, a home, the daemon, xclip, and the control socket.
// Each test binary uses part of this.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use jackstay_proto::control::{Request, Response};
use jackstayd::client::Client;

#[track_caller]
pub fn ok<T, E: std::fmt::Debug>(r: Result<T, E>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("expected Ok, got Err({e:?})"),
    }
}

/// An Xvfb of its own (see crates/x11-clip/tests/real/harness.rs for why it starts this way).
pub struct Xvfb {
    pub child: Child,
    pub display: String,
    _ready: BufReader<std::process::ChildStdout>,
}

static NEXT: AtomicU32 = AtomicU32::new(0);
static SOCKET_DIR: std::sync::Once = std::sync::Once::new();

impl Xvfb {
    pub fn start() -> Self {
        SOCKET_DIR.call_once(|| {
            let dir = Path::new("/tmp/.X11-unix");
            let _ = std::fs::create_dir(dir);
            let _ =
                std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o1777));
        });
        let number =
            100 + (std::process::id() % 200) * 256 + NEXT.fetch_add(1, Ordering::Relaxed) % 256;
        let script = format!(
            "exec setpriv --pdeathsig KILL Xvfb :{number} -displayfd 3 -nolisten tcp -screen 0 640x480x24 3>&1 1>/dev/null 2>/dev/null"
        );
        let mut child = ok(Command::new("sh").args(["-c", &script]).stdout(Stdio::piped()).spawn());
        let mut ready = BufReader::new(child.stdout.take().unwrap_or_else(|| panic!("no stdout")));
        let mut line = String::new();
        let started = ready.read_line(&mut line).is_ok() && line.trim() == number.to_string();
        let x = Xvfb { child, display: format!(":{number}"), _ready: ready };
        assert!(started, "Xvfb :{number} did not start: {line:?}");
        x
    }

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

/// A home of its own for one test: state, runtime, config and cache directories.
pub struct Home {
    pub root: PathBuf,
}

impl Home {
    pub fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        // Short: the socket's path must fit in 108 bytes, under however deep a tree.
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "d{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["state", "r", "config", "cache"] {
            ok(std::fs::create_dir_all(root.join(d)));
        }
        ok(std::fs::set_permissions(
            root.join("r"),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        ));
        let home = Home { root };
        let socket = home.socket().into_os_string().len();
        assert!(socket < 108, "socket path {socket} bytes long: run from a shallower tree");
        home
    }

    pub fn state(&self) -> PathBuf {
        self.root.join("state").join("jackstay")
    }

    pub fn runtime(&self) -> PathBuf {
        self.root.join("r").join("j")
    }

    pub fn socket(&self) -> PathBuf {
        self.runtime().join("control")
    }

    pub fn config(&self) -> PathBuf {
        self.root.join("config").join("config.toml")
    }
}

pub struct Daemon {
    pub child: Child,
}

impl Daemon {
    pub fn spawn(x: &Xvfb, home: &Home) -> Child {
        ok(x.command(env!("CARGO_BIN_EXE_jackstayd"))
            .args(["--display", &x.display])
            .arg("--state-dir")
            .arg(home.state())
            .arg("--runtime-dir")
            .arg(home.runtime())
            .arg("--config")
            .arg(home.config())
            .env("XDG_CACHE_HOME", home.root.join("cache"))
            .env_remove("NOTIFY_SOCKET")
            .stdout(Stdio::null())
            // Its journal, to read when a test fails.
            .stderr(ok(std::fs::File::options()
                .create(true)
                .append(true)
                .open(home.root.join("daemon.log"))))
            .spawn())
    }

    /// Starts the daemon and waits until its socket answers.
    pub fn start(x: &Xvfb, home: &Home) -> Self {
        let mut child = Self::spawn(x, home);
        let deadline = Instant::now() + Duration::from_secs(10);
        while Client::connect(&home.socket(), Duration::from_secs(5)).is_err() {
            if let Ok(Some(status)) = child.try_wait() {
                panic!("jackstayd exited at start: {status}");
            }
            assert!(Instant::now() < deadline, "jackstayd did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        Daemon { child }
    }

    /// Stops it as systemd does, and returns its exit status.
    pub fn stop(mut self) -> Option<i32> {
        let pid = i32::try_from(self.child.id()).unwrap_or(0);
        ok(Command::new("kill").args(["-TERM", &pid.to_string()]).status());
        let status = ok(self.child.wait());
        status.code()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The exit status of a daemon expected to refuse to start, within 10 s.
pub fn refused(mut child: Child) -> Option<i32> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = ok(child.try_wait()) {
            return status.code();
        }
        if Instant::now() >= deadline {
            kill(child);
            panic!("jackstayd started instead of refusing");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[track_caller]
pub fn client(home: &Home) -> Client {
    ok(Client::connect(&home.socket(), Duration::from_secs(10)))
}

#[track_caller]
pub fn ask(home: &Home, r: Request) -> Response {
    ok(client(home).ask(&r))
}

pub fn copy(x: &Xvfb, text: &str) -> Child {
    let mut child = ok(x
        .command("xclip")
        .args(["-quiet", "-selection", "clipboard", "-target", "UTF8_STRING", "-i"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn());
    let mut stdin = child.stdin.take().unwrap_or_else(|| panic!("no stdin"));
    ok(stdin.write_all(text.as_bytes()));
    drop(stdin);
    child
}

pub fn paste(x: &Xvfb) -> Option<String> {
    let out = ok(x
        .command("timeout")
        .args(["10", "xclip", "-selection", "clipboard", "-target", "UTF8_STRING", "-o"])
        .stderr(Stdio::null())
        .output());
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
        .filter(|s| !s.is_empty())
}

pub fn kill(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// The previews `jackstay list` would show, newest first.
#[track_caller]
pub fn previews(home: &Home) -> Vec<String> {
    match ask(home, Request::List { offset: 0, limit: 100 }) {
        Response::List { entries, .. } => entries.into_iter().map(|e| e.preview).collect(),
        other => panic!("not a list: {other:?}"),
    }
}

/// As [`previews`], or `None` if the daemon could not be asked: one that has just been full
/// closes a newcomer until it has seen the others go.
pub fn try_previews(home: &Home) -> Option<Vec<String>> {
    let mut c = Client::connect(&home.socket(), Duration::from_secs(10)).ok()?;
    match c.ask(&Request::List { offset: 0, limit: 100 }).ok()? {
        Response::List { entries, .. } => Some(entries.into_iter().map(|e| e.preview).collect()),
        _ => None,
    }
}

pub fn ids(home: &Home) -> Vec<u64> {
    match ask(home, Request::List { offset: 0, limit: 100 }) {
        Response::List { entries, .. } => entries.into_iter().map(|e| e.id).collect(),
        other => panic!("not a list: {other:?}"),
    }
}

/// Waits up to 10 s for `check`.
#[track_caller]
pub fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub fn blob_count(home: &Home) -> usize {
    std::fs::read_dir(home.state().join("blobs")).map_or(0, Iterator::count)
}
