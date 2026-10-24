//! The daemon end to end: the real `jackstayd` binary on a private Xvfb, driven with xclip and the
//! control socket, its state in a directory of its own.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use jackstay_proto::control::{Error, Request, Response};
use jackstayd::client::Client;

fn ok<T, E: std::fmt::Debug>(r: Result<T, E>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("expected Ok, got Err({e:?})"),
    }
}

/// An Xvfb of its own (see crates/x11-clip/tests/real/harness.rs for why it starts this way).
struct Xvfb {
    child: Child,
    display: String,
    _ready: BufReader<std::process::ChildStdout>,
}

static NEXT: AtomicU32 = AtomicU32::new(0);
static SOCKET_DIR: std::sync::Once = std::sync::Once::new();

impl Xvfb {
    fn start() -> Self {
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

    fn command(&self, program: &str) -> Command {
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
struct Home {
    root: PathBuf,
}

impl Home {
    fn new() -> Self {
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

    fn state(&self) -> PathBuf {
        self.root.join("state").join("jackstay")
    }

    fn runtime(&self) -> PathBuf {
        self.root.join("r").join("j")
    }

    fn socket(&self) -> PathBuf {
        self.runtime().join("control")
    }

    fn config(&self) -> PathBuf {
        self.root.join("config").join("config.toml")
    }
}

struct Daemon {
    child: Child,
}

impl Daemon {
    fn spawn(x: &Xvfb, home: &Home) -> Child {
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
            .stderr(Stdio::null())
            .spawn())
    }

    /// Starts the daemon and waits until its socket answers.
    fn start(x: &Xvfb, home: &Home) -> Self {
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
    fn stop(mut self) -> Option<i32> {
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
fn refused(mut child: Child) -> Option<i32> {
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

fn client(home: &Home) -> Client {
    ok(Client::connect(&home.socket(), Duration::from_secs(10)))
}

fn ask(home: &Home, r: Request) -> Response {
    ok(client(home).ask(&r))
}

fn copy(x: &Xvfb, text: &str) -> Child {
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

fn paste(x: &Xvfb) -> Option<String> {
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

fn kill(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// The previews `jackstay list` would show, newest first.
fn previews(home: &Home) -> Vec<String> {
    match ask(home, Request::List { offset: 0, limit: 100 }) {
        Response::List { entries, .. } => entries.into_iter().map(|e| e.preview).collect(),
        other => panic!("not a list: {other:?}"),
    }
}

fn ids(home: &Home) -> Vec<u64> {
    match ask(home, Request::List { offset: 0, limit: 100 }) {
        Response::List { entries, .. } => entries.into_iter().map(|e| e.id).collect(),
        other => panic!("not a list: {other:?}"),
    }
}

/// Waits up to 10 s for `check`.
fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn blob_count(home: &Home) -> usize {
    std::fs::read_dir(home.state().join("blobs")).map_or(0, Iterator::count)
}

#[test]
fn copies_are_kept_listed_and_outlive_their_owner() {
    let x = Xvfb::start();
    let home = Home::new();
    let d = Daemon::start(&x, &home);
    let owner = copy(&x, "first copy");
    eventually("the copy in the history", || previews(&home) == ["first copy"]);
    kill(owner);
    eventually("the copy kept alive", || paste(&x).as_deref() == Some("first copy"));
    let second = copy(&x, "second\tcopy\nwith lines");
    eventually("the second copy", || {
        previews(&home).first().map(String::as_str) == Some("second copy with lines")
    });
    kill(second);
    assert_eq!(d.stop(), Some(0));
}

#[test]
fn clear_empties_the_clipboard_and_deletes_the_entry_from_disk() {
    let x = Xvfb::start();
    let home = Home::new();
    let d = Daemon::start(&x, &home);
    let keep = copy(&x, "keep me");
    eventually("the first copy", || previews(&home) == ["keep me"]);
    let secret = copy(&x, "hunter2");
    eventually("the secret", || previews(&home).len() == 2);
    eventually("both saved", || blob_count(&home) == 2);
    assert_eq!(ask(&home, Request::Clear), Response::Ok);
    assert_eq!(paste(&x), None);
    assert_eq!(previews(&home), ["keep me"]);
    // Acknowledged only once on disk: no blob of the secret remains.
    assert_eq!(blob_count(&home), 1);
    let index = ok(std::fs::read_to_string(home.state().join("index")));
    assert!(index.contains("cleared 1"));
    kill(keep);
    kill(secret);
    assert_eq!(d.stop(), Some(0));
}

#[test]
fn select_pin_and_delete() {
    let x = Xvfb::start();
    let home = Home::new();
    let d = Daemon::start(&x, &home);
    let a = copy(&x, "alpha");
    eventually("alpha", || previews(&home) == ["alpha"]);
    let b = copy(&x, "beta");
    eventually("beta", || previews(&home).len() == 2);
    let [beta, alpha] = <[u64; 2]>::try_from(ids(&home)).unwrap_or_else(|v| panic!("{v:?}"));
    assert_eq!(ask(&home, Request::Select { id: alpha }), Response::Ok);
    assert_eq!(paste(&x).as_deref(), Some("alpha"));
    assert_eq!(previews(&home), ["alpha", "beta"]);
    assert_eq!(ask(&home, Request::Pin { id: alpha, pinned: true }), Response::Ok);
    // Clearing keeps the pinned entry on the clipboard's list; deleting it removes it anyway.
    assert_eq!(ask(&home, Request::Clear), Response::Ok);
    assert_eq!(previews(&home), ["alpha", "beta"]);
    assert_eq!(ask(&home, Request::Delete { id: beta }), Response::Ok);
    assert_eq!(previews(&home), ["alpha"]);
    assert_eq!(ask(&home, Request::Delete { id: beta }), Response::Error(Error::NoSuchEntry));
    assert_eq!(ask(&home, Request::Select { id: alpha }), Response::Ok);
    assert_eq!(ask(&home, Request::Delete { id: alpha }), Response::Ok);
    assert_eq!(paste(&x), None, "deleting the entry on the clipboard clears it");
    kill(a);
    kill(b);
    assert_eq!(d.stop(), Some(0));
}

#[test]
fn a_restart_restores_the_newest_entry_unless_cleared() {
    let x = Xvfb::start();
    let home = Home::new();
    let d = Daemon::start(&x, &home);
    let owner = copy(&x, "survives a restart");
    eventually("the copy", || previews(&home) == ["survives a restart"]);
    kill(owner);
    eventually("kept alive", || paste(&x).as_deref() == Some("survives a restart"));
    assert_eq!(d.stop(), Some(0));
    eventually("nothing on the clipboard", || paste(&x).is_none());
    let d = Daemon::start(&x, &home);
    eventually("restored", || paste(&x).as_deref() == Some("survives a restart"));
    assert_eq!(ask(&home, Request::Clear), Response::Ok);
    assert_eq!(d.stop(), Some(0));
    let d = Daemon::start(&x, &home);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(paste(&x), None, "a cleared clipboard stays cleared across a restart");
    assert_eq!(d.stop(), Some(0));
}

#[test]
fn a_live_copy_is_not_overwritten_by_a_restore() {
    let x = Xvfb::start();
    let home = Home::new();
    let d = Daemon::start(&x, &home);
    let owner = copy(&x, "saved");
    eventually("the copy", || previews(&home) == ["saved"]);
    kill(owner);
    assert_eq!(d.stop(), Some(0));
    let live = copy(&x, "live");
    eventually("the live copy", || paste(&x).as_deref() == Some("live"));
    let d = Daemon::start(&x, &home);
    eventually("the live copy captured", || {
        previews(&home).first().map(String::as_str) == Some("live")
    });
    assert_eq!(paste(&x).as_deref(), Some("live"));
    kill(live);
    assert_eq!(d.stop(), Some(0));
}

#[test]
fn clipmans_history_is_imported_once_behind_jackstays() {
    let x = Xvfb::start();
    let home = Home::new();
    let dir = home.root.join("cache").join("xfce4").join("clipman");
    ok(std::fs::create_dir_all(&dir));
    ok(std::fs::write(dir.join("textsrc"), "[texts]\ntexts=oldest;newest;\n"));
    let d = Daemon::start(&x, &home);
    assert_eq!(previews(&home), ["newest", "oldest"]);
    let owner = copy(&x, "mine");
    eventually("my copy", || previews(&home) == ["mine", "newest", "oldest"]);
    assert_eq!(
        ask(&home, Request::ImportClipman { remove: true }),
        Response::Imported { added: 0, skipped: 2 }
    );
    assert!(!dir.join("textsrc").exists(), "--remove deletes clipman's files");
    kill(owner);
    assert_eq!(d.stop(), Some(0));
    let d = Daemon::start(&x, &home);
    assert_eq!(previews(&home), ["mine", "newest", "oldest"], "imported once");
    assert_eq!(d.stop(), Some(0));
}

#[test]
fn a_second_instance_and_a_bad_configuration_are_refused() {
    let x = Xvfb::start();
    let home = Home::new();
    let d = Daemon::start(&x, &home);
    assert_eq!(refused(Daemon::spawn(&x, &home)), Some(3));
    // Nor may one with its own runtime directory, on another display (so it is no rival for
    // CLIPBOARD_MANAGER), share the history.
    let other_run = home.root.join("r2");
    ok(std::fs::create_dir_all(&other_run));
    let other_x = Xvfb::start();
    let sharing = ok(other_x
        .command(env!("CARGO_BIN_EXE_jackstayd"))
        .args(["--display", &other_x.display])
        .arg("--state-dir")
        .arg(home.state())
        .arg("--runtime-dir")
        .arg(other_run.join("j"))
        .arg("--config")
        .arg(home.config())
        .env("XDG_CACHE_HOME", home.root.join("cache"))
        .stderr(Stdio::null())
        .spawn());
    assert_eq!(refused(sharing), Some(3));
    assert_eq!(d.stop(), Some(0));
    ok(std::fs::write(home.config(), "[history]\nmax_entires = 5\n"));
    assert_eq!(refused(Daemon::spawn(&x, &home)), Some(2));
}

#[test]
fn the_protocol_is_enforced_per_client() {
    let x = Xvfb::start();
    let home = Home::new();
    let d = Daemon::start(&x, &home);
    // A request before Hello is refused and the connection closed.
    let mut raw = ok(std::os::unix::net::UnixStream::connect(home.socket()));
    let mut out = Vec::new();
    jackstay_proto::control::encode_request(&Request::Status, &mut out);
    ok(raw.write_all(&out));
    let mut buf = Vec::new();
    ok(std::io::Read::read_to_end(&mut raw, &mut buf));
    assert_eq!(
        jackstay_proto::control::decode_response(&buf).map(|r| r.map(|(r, _)| r)),
        Ok(Some(Response::Error(Error::BadSequence)))
    );
    // A malformed frame closes only that client.
    let mut garbage = ok(std::os::unix::net::UnixStream::connect(home.socket()));
    ok(garbage.set_read_timeout(Some(Duration::from_secs(5))));
    ok(garbage.write_all(&[0, 0, 0, 0, 200]));
    let mut buf = Vec::new();
    ok(std::io::Read::read_to_end(&mut garbage, &mut buf));
    assert!(buf.is_empty(), "closed without a response");
    assert!(matches!(ask(&home, Request::Status), Response::Status(_)));
    // A subscriber hears about changes.
    let mut sub = client(&home);
    assert_eq!(ok(sub.ask(&Request::Subscribe)), Response::Ok);
    let owner = copy(&x, "news");
    assert_eq!(ok(sub.receive()), Response::Changed);
    kill(owner);
    assert_eq!(d.stop(), Some(0));
}

/// The daemon's CPU time so far, in clock ticks.
fn cpu_ticks(pid: u32) -> u64 {
    let stat = ok(std::fs::read_to_string(format!("/proc/{pid}/stat")));
    // Fields after the command name, which is in parentheses and may hold spaces.
    let rest = stat.rsplit_once(')').map_or("", |(_, r)| r);
    let fields: Vec<&str> = rest.split_whitespace().collect();
    fields.get(11).and_then(|u| u.parse::<u64>().ok()).unwrap_or(0)
        + fields.get(12).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0)
}

fn raw(home: &Home) -> std::os::unix::net::UnixStream {
    let s = ok(std::os::unix::net::UnixStream::connect(home.socket()));
    ok(s.set_read_timeout(Some(Duration::from_secs(10))));
    s
}

fn frames(requests: &[Request]) -> Vec<u8> {
    let mut out = Vec::new();
    for r in requests {
        jackstay_proto::control::encode_request(r, &mut out);
    }
    out
}

/// Reads `n` responses from a raw connection.
fn responses(s: &mut std::os::unix::net::UnixStream, n: usize) -> Vec<Response> {
    let mut input = Vec::new();
    let mut got = Vec::new();
    while got.len() < n {
        match jackstay_proto::control::decode_response(&input) {
            Ok(Some((r, used))) => {
                input.drain(..used);
                got.push(r);
            }
            Ok(None) => {
                let mut buf = [0u8; 4096];
                let k = ok(std::io::Read::read(s, &mut buf));
                assert!(k > 0, "closed after {} responses: {got:?}", got.len());
                input.extend_from_slice(&buf[..k]);
            }
            Err(e) => panic!("bad response: {e}"),
        }
    }
    got
}

#[test]
fn a_stalled_or_flooding_client_costs_the_daemon_nothing() {
    let x = Xvfb::start();
    let home = Home::new();
    let d = Daemon::start(&x, &home);
    let pid = d.child.id();
    // Half a frame, then nothing: the loop must still sleep.
    let mut half = raw(&home);
    ok(half.write_all(&frames(&[Request::Hello { version: jackstay_proto::control::VERSION }])));
    ok(half.write_all(&[0, 0]));
    std::thread::sleep(Duration::from_millis(300));
    let before = cpu_ticks(pid);
    std::thread::sleep(Duration::from_secs(2));
    let used = cpu_ticks(pid) - before;
    assert!(used < 20, "the daemon spun: {used} ticks in 2 s");
    // A client that floods requests and never reads stalls only itself.
    let mut flood = raw(&home);
    let mut many = vec![Request::Hello { version: jackstay_proto::control::VERSION }];
    many.extend(std::iter::repeat_n(Request::Status, 20_000));
    let bytes = frames(&many);
    let writer = std::thread::spawn(move || {
        let _ = flood.write_all(&bytes);
        flood
    });
    std::thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    assert!(matches!(ask(&home, Request::Status), Response::Status(_)));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "another client waited {:?}",
        started.elapsed()
    );
    drop(half);
    drop(writer);
    assert_eq!(d.stop(), Some(0));
}

#[test]
fn at_most_sixteen_clients_and_pipelined_requests_keep_their_order() {
    let x = Xvfb::start();
    let home = Home::new();
    let d = Daemon::start(&x, &home);
    let held: Vec<Client> = (0..16).map(|_| client(&home)).collect();
    let mut extra = raw(&home);
    let mut buf = Vec::new();
    ok(std::io::Read::read_to_end(&mut extra, &mut buf));
    assert!(buf.is_empty(), "the 17th is closed at once");
    drop(held);
    // A clear (answered once the store confirms) and a status sent together: answered in order.
    let owner = copy(&x, "secret");
    eventually("the copy", || previews(&home) == ["secret"]);
    let mut s = raw(&home);
    let hello = Request::Hello { version: jackstay_proto::control::VERSION };
    ok(s.write_all(&frames(&[hello, Request::Clear, Request::Status])));
    let got = responses(&mut s, 3);
    assert!(
        matches!(got.as_slice(), [Response::Welcome { .. }, Response::Ok, Response::Status(_)]),
        "{got:?}"
    );
    // A client that sends a clear and closes its end still has it done and answered.
    let again = copy(&x, "another secret");
    eventually("the copy", || previews(&home) == ["another secret"]);
    let mut s = raw(&home);
    ok(s.write_all(&frames(&[hello, Request::Clear])));
    ok(s.shutdown(std::net::Shutdown::Write));
    let got = responses(&mut s, 2);
    assert_eq!(got.get(1), Some(&Response::Ok));
    assert_eq!(previews(&home), Vec::<String>::new());
    kill(owner);
    kill(again);
    assert_eq!(d.stop(), Some(0));
}

#[test]
fn stopping_saves_the_latest_copy() {
    let x = Xvfb::start();
    let home = Home::new();
    let d = Daemon::start(&x, &home);
    let first = copy(&x, "first");
    eventually("first", || previews(&home) == ["first"]);
    let second = copy(&x, "copied just before stopping");
    eventually("second", || previews(&home).len() == 2);
    assert_eq!(d.stop(), Some(0), "stopped well within the second ordinary saves wait");
    kill(first);
    kill(second);
    let d = Daemon::start(&x, &home);
    assert_eq!(previews(&home), ["copied just before stopping", "first"]);
    assert_eq!(d.stop(), Some(0));
}

#[test]
fn losing_the_x_server_exits_4() {
    let x = Xvfb::start();
    let home = Home::new();
    let mut d = Daemon::start(&x, &home);
    drop(x);
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = ok(d.child.try_wait()) {
            break status;
        }
        assert!(Instant::now() < deadline, "jackstayd did not notice");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(status.code(), Some(4));
}

fn status(home: &Home) -> jackstay_proto::control::Status {
    match ask(home, Request::Status) {
        Response::Status(s) => s,
        other => panic!("not a status: {other:?}"),
    }
}

#[test]
fn an_unreadable_clipman_history_is_imported_once_it_can_be_read() {
    let x = Xvfb::start();
    let home = Home::new();
    let dir = home.root.join("cache").join("xfce4").join("clipman");
    ok(std::fs::create_dir_all(&dir));
    ok(std::fs::write(dir.join("textsrc"), "not a key file"));
    let d = Daemon::start(&x, &home);
    assert_eq!(previews(&home), Vec::<String>::new());
    assert_eq!(d.stop(), Some(0));
    ok(std::fs::write(dir.join("textsrc"), "[texts]\ntexts=clipman's;\n"));
    let d = Daemon::start(&x, &home);
    assert_eq!(previews(&home), ["clipman's"], "tried again at the next start");
    assert_eq!(ask(&home, Request::Delete { id: ids(&home)[0] }), Response::Ok);
    assert_eq!(d.stop(), Some(0));
    let d = Daemon::start(&x, &home);
    assert_eq!(previews(&home), Vec::<String>::new(), "imported once, so a deletion stands");
    assert_eq!(d.stop(), Some(0));
}

#[test]
fn an_import_is_saved_without_waiting_for_a_stop() {
    let x = Xvfb::start();
    let home = Home::new();
    let d = Daemon::start(&x, &home);
    let dir = home.root.join("cache").join("xfce4").join("clipman");
    ok(std::fs::create_dir_all(&dir));
    ok(std::fs::write(dir.join("textsrc"), "[texts]\ntexts=later;\n"));
    assert_eq!(
        ask(&home, Request::ImportClipman { remove: false }),
        Response::Imported { added: 1, skipped: 0 }
    );
    // Past the ordinary save's wait; then killed, so only what was saved by now survives.
    std::thread::sleep(Duration::from_secs(2));
    drop(d);
    let d = Daemon::start(&x, &home);
    assert_eq!(previews(&home), ["later"]);
    assert_eq!(d.stop(), Some(0));
}

#[test]
fn status_and_list_tell_the_truth_and_a_failing_store_is_reported() {
    let x = Xvfb::start();
    let home = Home::new();
    let d = Daemon::start(&x, &home);
    let first = copy(&x, "first");
    eventually("first", || previews(&home) == ["first"]);
    let second = copy(&x, "second");
    eventually("second", || previews(&home) == ["second", "first"]);
    let entries = match ask(&home, Request::List { offset: 0, limit: 10 }) {
        Response::List { entries, .. } => entries,
        other => panic!("not a list: {other:?}"),
    };
    assert_eq!(entries.iter().map(|e| e.current).collect::<Vec<_>>(), [true, false]);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0));
    assert!(entries.iter().all(|e| e.copied_at_ms.abs_diff(now) < 60_000), "copied just now");
    assert!(!status(&home).store_failing);
    // Where blobs go, a file: no save can succeed, whoever runs the test.
    let blobs = home.state().join("blobs");
    let away = home.state().join("blobs.away");
    ok(std::fs::rename(&blobs, &away));
    ok(std::fs::write(&blobs, b""));
    let third = copy(&x, "third");
    eventually("the failure reported", || status(&home).store_failing);
    ok(std::fs::remove_file(&blobs));
    ok(std::fs::rename(&away, &blobs));
    eventually("saving again", || !status(&home).store_failing);
    kill(first);
    kill(second);
    kill(third);
    assert_eq!(d.stop(), Some(0));
}

#[test]
fn an_entry_changed_on_disk_is_dropped_and_taken_off_the_clipboard() {
    let x = Xvfb::start();
    let home = Home::new();
    let d = Daemon::start(&x, &home);
    let owner = copy(&x, "tampered");
    eventually("the copy", || previews(&home) == ["tampered"]);
    kill(owner);
    assert_eq!(d.stop(), Some(0));
    for blob in ok(std::fs::read_dir(home.state().join("blobs"))) {
        let path = ok(blob).path();
        let mut bytes = ok(std::fs::read(&path));
        for b in &mut bytes {
            *b ^= 0x20;
        }
        ok(std::fs::write(&path, bytes));
    }
    let d = Daemon::start(&x, &home);
    eventually("the entry dropped", || previews(&home).is_empty());
    eventually("the clipboard cleared", || paste(&x).is_none());
    assert_eq!(d.stop(), Some(0));
}
