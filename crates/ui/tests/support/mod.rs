//! What the UI's end-to-end tests share: an Xvfb, a stand-in daemon that records what it is
//! asked, and the real `jackstay-ui` driven with xdotool.
// Each test binary uses part of this.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read as _, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use jackstay_proto::control::{self, Kinds, Request, Response, Summary, Thumb, VERSION};
use jackstayd::ui_socket;

#[track_caller]
pub fn ok<T, E: std::fmt::Debug>(r: Result<T, E>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("{e:?}"),
    }
}

#[track_caller]
pub fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// An Xvfb of its own, as the daemon's tests start one.
pub struct Xvfb {
    child: Child,
    pub display: String,
    _ready: BufReader<std::process::ChildStdout>,
}

impl Xvfb {
    pub fn start() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let _ = std::fs::create_dir("/tmp/.X11-unix");
        let number =
            300 + (std::process::id() % 200) * 64 + NEXT.fetch_add(1, Ordering::Relaxed) % 64;
        // -noreset: a server whose last client disconnects resets, and closes connections made
        // meanwhile (xfsettingsd's, under load, after a test's own query: CI, 2026-10-27).
        let script = format!(
            "exec setpriv --pdeathsig KILL Xvfb :{number} -displayfd 3 -noreset -nolisten tcp -screen 0 1024x768x24 3>&1 1>/dev/null 2>/dev/null"
        );
        let mut child = ok(Command::new("sh").args(["-c", &script]).stdout(Stdio::piped()).spawn());
        let mut ready = BufReader::new(child.stdout.take().unwrap_or_else(|| panic!("no stdout")));
        let mut line = String::new();
        let started = ready.read_line(&mut line).is_ok() && line.trim() == number.to_string();
        let x = Xvfb { child, display: format!(":{number}"), _ready: ready };
        assert!(started, "Xvfb did not start: {line:?}");
        x
    }

    pub fn command(&self, program: &str) -> Command {
        let mut c = Command::new(program);
        c.env("DISPLAY", &self.display);
        c
    }

    pub fn xdotool(&self, args: &[&str]) {
        assert!(ok(self.command("xdotool").args(args).status()).success(), "xdotool {args:?}");
    }
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn entry(id: u64, preview: &str, current: bool) -> Summary {
    Summary {
        id,
        copied_at_ms: 0,
        pinned: false,
        current,
        kinds: Kinds(Kinds::TEXT),
        size: preview.len() as u64,
        source: Some("Firefox".to_owned()),
        preview: preview.to_owned(),
    }
}

/// A daemon that lists `entries` (newest first), drops what is deleted, records every request,
/// and answers the rest with `Ok`.
#[derive(Clone)]
pub struct Daemon {
    asked: Arc<Mutex<Vec<Request>>>,
    /// What each thumbnail request is answered, in turn (`None`: an error); the last one again
    /// once the rest are used, and `Absent` for an entry not here.
    thumbnails: Arc<Mutex<HashMap<u64, Vec<Option<Thumb>>>>>,
}

impl Daemon {
    pub fn start(runtime: &Path, entries: Vec<Summary>) -> Self {
        Self::answering(runtime, entries, Response::Ok)
    }

    /// As [`Daemon::start`], answering a paste (`Select`) with `select` instead.
    pub fn answering(runtime: &Path, entries: Vec<Summary>, select: Response) -> Self {
        Self::with(runtime, entries, select, Response::Ok)
    }

    /// As [`Daemon::start`], refusing every delete.
    pub fn refusing_deletes(runtime: &Path, entries: Vec<Summary>) -> Self {
        let no = Response::Error(jackstay_proto::control::Error::NoSuchEntry);
        Self::with(runtime, entries, Response::Ok, no)
    }

    pub fn with(runtime: &Path, entries: Vec<Summary>, select: Response, delete: Response) -> Self {
        let listener = ok(UnixListener::bind(runtime.join("control")));
        let asked = Arc::new(Mutex::new(Vec::new()));
        let thumbnails = Arc::new(Mutex::new(HashMap::new()));
        let entries = Arc::new(Mutex::new(entries));
        let (log, thumbs) = (Arc::clone(&asked), Arc::clone(&thumbnails));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (log, entries, thumbs) =
                    (Arc::clone(&log), Arc::clone(&entries), Arc::clone(&thumbs));
                let (select, delete) = (select.clone(), delete.clone());
                std::thread::spawn(move || {
                    serve(stream, &log, &entries, &thumbs, &select, &delete);
                });
            }
        });
        Daemon { asked, thumbnails }
    }

    /// Answers entry `id`'s thumbnail requests with `answers`, in turn.
    pub fn thumbnail(&self, id: u64, answers: Vec<Option<Thumb>>) {
        if let Ok(mut t) = self.thumbnails.lock() {
            t.insert(id, answers);
        }
    }

    /// The requests so far, other than listing and thumbnails.
    pub fn changes(&self) -> Vec<Request> {
        let asked = self.asked.lock().map(|a| a.clone()).unwrap_or_default();
        asked
            .into_iter()
            .filter(|r| !matches!(r, Request::List { .. } | Request::Thumbnail { .. }))
            .collect()
    }

    /// The thumbnail requests so far, by entry.
    pub fn thumbnails_asked(&self) -> Vec<u64> {
        let asked = self.asked.lock().map(|a| a.clone()).unwrap_or_default();
        asked
            .into_iter()
            .filter_map(|r| match r {
                Request::Thumbnail { id } => Some(id),
                _ => None,
            })
            .collect()
    }

    pub fn lists(&self) -> usize {
        let asked = self.asked.lock().map(|a| a.clone()).unwrap_or_default();
        asked.iter().filter(|r| matches!(r, Request::List { .. })).count()
    }
}

pub fn serve(
    mut s: UnixStream,
    log: &Mutex<Vec<Request>>,
    entries: &Mutex<Vec<Summary>>,
    thumbnails: &Mutex<HashMap<u64, Vec<Option<Thumb>>>>,
    select: &Response,
    delete: &Response,
) {
    let mut input = Vec::new();
    loop {
        let Ok(decoded) = control::decode_request(&input) else { return };
        let Some((request, used)) = decoded else {
            let mut buf = [0u8; 4096];
            match s.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => input.extend_from_slice(&buf[..n]),
            }
            continue;
        };
        input.drain(..used);
        let answer = match &request {
            Request::Hello { .. } => Response::Welcome { version: VERSION },
            Request::List { .. } => {
                let e = entries.lock().map(|e| e.clone()).unwrap_or_default();
                Response::List { total: u32::try_from(e.len()).unwrap_or(0), entries: e }
            }
            Request::Delete { id } => {
                if *delete == Response::Ok
                    && let Ok(mut e) = entries.lock()
                {
                    e.retain(|s| s.id != *id);
                }
                delete.clone()
            }
            Request::Select { .. } => select.clone(),
            Request::Thumbnail { id } => {
                let mut t = thumbnails.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let answers = t.entry(*id).or_default();
                let answer = if answers.len() > 1 {
                    answers.remove(0)
                } else {
                    answers.first().cloned().unwrap_or(Some(Thumb::Absent))
                };
                match answer {
                    Some(thumbnail) => Response::Thumbnail { id: *id, thumbnail },
                    None => Response::Error(control::Error::Internal),
                }
            }
            _ => Response::Ok,
        };
        if !matches!(request, Request::Hello { .. })
            && let Ok(mut l) = log.lock()
        {
            l.push(request);
        }
        let mut out = Vec::new();
        ok(control::encode_response(&answer, &mut out));
        if s.write_all(&out).is_err() {
            return;
        }
    }
}

pub struct Ui {
    pub x: Xvfb,
    pub runtime: PathBuf,
    pub child: Child,
}

impl Drop for Ui {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn runtime(name: &str) -> PathBuf {
    let d =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("ui-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    ok(std::fs::create_dir_all(&d));
    ok(std::fs::set_permissions(&d, std::os::unix::fs::PermissionsExt::from_mode(0o700)));
    d
}

pub fn ui_command(x: &Xvfb, runtime: &Path) -> Command {
    let mut c = x.command(env!("CARGO_BIN_EXE_jackstay-ui"));
    c.arg("--runtime-dir")
        .arg(runtime)
        .env("GDK_BACKEND", "x11")
        .env("NO_AT_BRIDGE", "1")
        .env("GTK_A11Y", "none")
        .env("LANG", "C.UTF-8")
        .env("DBUS_SESSION_BUS_ADDRESS", "disabled:")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    c
}

pub fn start(name: &str) -> Ui {
    start_with(name, |_| {})
}

/// As [`start`], with `adjust` applied to the UI's command first.
pub fn start_with(name: &str, adjust: impl FnOnce(&mut Command)) -> Ui {
    let x = Xvfb::start();
    let runtime = runtime(name);
    let mut command = ui_command(&x, &runtime);
    adjust(&mut command);
    let child = ok(command.spawn());
    let path = ui_socket::path(&runtime);
    eventually("the UI's socket", || path.exists());
    Ui { x, runtime, child }
}

impl Ui {
    pub fn popup(&self) {
        ok(ui_socket::request(&self.runtime, ui_socket::Ask::Popup, Duration::from_secs(5)));
        eventually("the popup", || self.visible());
    }

    pub fn visible(&self) -> bool {
        let out = ok(self
            .x
            .command("xdotool")
            .args(["search", "--onlyvisible", "--name", "Jackstay history"])
            .output());
        out.status.success()
    }

    pub fn keys(&self, keys: &[&str]) {
        for k in keys {
            self.x.xdotool(&["key", k]);
            // Each key's effect lands before the next, as a person's would.
            std::thread::sleep(Duration::from_millis(60));
        }
    }

    pub fn closed(&self) {
        eventually("the popup to close", || !self.visible());
    }
}

pub fn history() -> Vec<Summary> {
    vec![
        entry(14, "newest copy", true),
        entry(13, "a password: hunter2", false),
        entry(12, "third\nline two", false),
        entry(11, "oldest", false),
    ]
}
