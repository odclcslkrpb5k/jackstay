//! The command line's exit statuses, and `popup` until the popup exists, against a stand-in
//! daemon.

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use jackstay_proto::control::{
    self, Error, Kinds, LinkStatus, PairReason, PeerState, PeerSummary, Request, Response, Status,
    Summary, VERSION,
};

#[track_caller]
fn ok<T, E: std::fmt::Debug>(r: Result<T, E>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("{e:?}"),
    }
}

/// A directory of its own for one test. Short: a socket's path in it must fit in 108 bytes, under
/// however deep a tree and with however long a process id.
fn dir() -> PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("c{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    ok(std::fs::create_dir_all(&d));
    let socket = d.join("control").into_os_string().len();
    assert!(socket < 108, "socket path {socket} bytes long: run from a shallower tree");
    d
}

/// `jackstay` with `args`, no display unless `display`, and `bin` first on its `PATH`.
fn jackstay(args: &[&str], display: bool, bin: Option<&Path>) -> Output {
    jackstay_with_input(args, display, bin, None)
}

/// As [`jackstay`], with `input` on its standard input (none: closed at once).
fn jackstay_with_input(
    args: &[&str],
    display: bool,
    bin: Option<&Path>,
    input: Option<&str>,
) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_jackstay"));
    c.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    c.args(args).env_remove("DISPLAY");
    if display {
        c.env("DISPLAY", ":99");
    }
    if let Some(bin) = bin {
        let mut path = bin.as_os_str().to_owned();
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap_or_default());
        c.env("PATH", path);
    }
    let mut child = ok(c.spawn());
    if let Some(mut stdin) = child.stdin.take() {
        ok(stdin.write_all(input.unwrap_or_default().as_bytes()));
    }
    ok(child.wait_with_output())
}

#[test]
fn bad_usage_is_2_and_no_daemon_is_3() {
    assert_eq!(jackstay(&[], false, None).status.code(), Some(2));
    assert_eq!(jackstay(&["select", "x"], false, None).status.code(), Some(2));
    let empty = dir();
    let runtime = empty.to_str().unwrap_or_default();
    assert_eq!(jackstay(&["--runtime-dir", runtime, "status"], false, None).status.code(), Some(3));
    let help = jackstay(&["help"], false, None);
    assert!(String::from_utf8_lossy(&help.stdout).contains("popup"));
}

/// `popup` and `settings` ask the running jackstay-ui; without one they fail with status 3, and say
/// so as a desktop notification too, since run from a hotkey or a menu nothing shows stderr.
#[test]
fn popup_and_settings_ask_the_ui_and_say_so_where_a_user_sees_it_if_none_runs() {
    let root = dir();
    let bin = root.join("bin");
    ok(std::fs::create_dir_all(&bin));
    let said = root.join("notified");
    let notify = bin.join("notify-send");
    ok(std::fs::write(
        &notify,
        format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", said.display()),
    ));
    ok(std::fs::set_permissions(&notify, std::os::unix::fs::PermissionsExt::from_mode(0o755)));
    let runtime = root.to_str().unwrap_or_default();
    // A UI that answers each of two requests.
    let listener = ok(UnixListener::bind(root.join("ui")));
    let ui = std::thread::spawn(move || {
        let mut asked = Vec::new();
        for _ in 0..2 {
            let (mut s, _) = ok(listener.accept());
            let mut byte = [0u8];
            while asked.last() != Some(&b'\n') {
                ok(s.read_exact(&mut byte));
                asked.push(byte[0]);
            }
            ok(s.write_all(b"ok\n"));
            asked.push(b'|');
        }
        asked
    });
    for window in ["popup", "settings"] {
        let out = jackstay(&["--runtime-dir", runtime, window], true, Some(&bin));
        assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    }
    assert_eq!(&ok(ui.join().map_err(|_| "ui")), b"popup\n|settings\n|");
    assert!(!said.exists(), "nothing to tell");
    // None.
    let _ = std::fs::remove_file(root.join("ui"));
    for window in ["popup", "settings"] {
        let _ = std::fs::remove_file(&said);
        let out = jackstay(&["--runtime-dir", runtime, window], true, Some(&bin));
        assert_eq!(out.status.code(), Some(3));
        let notified = ok(std::fs::read_to_string(&said));
        assert!(notified.starts_with("--app-name=Jackstay\nJackstay\n"), "{notified}");
        assert!(notified.contains("jackstay-ui is not running"), "{notified}");
    }
    // Another command's failure is not announced.
    let _ = std::fs::remove_file(&said);
    let _ = jackstay(&["--runtime-dir", runtime, "status"], true, Some(&bin));
    assert!(!said.exists());
}

/// A daemon that greets one client, then answers each request it sends with the next of
/// `answers`; returns the requests it got.
fn scripted(runtime: &Path, answers: Vec<Response>) -> std::thread::JoinHandle<Vec<Request>> {
    scripted_batches(runtime, answers.into_iter().map(|a| vec![a]).collect())
}

/// As [`scripted`], with each request answered by a batch: a flow's response and the events
/// that follow it.
fn scripted_batches(
    runtime: &Path,
    answers: Vec<Vec<Response>>,
) -> std::thread::JoinHandle<Vec<Request>> {
    let listener = ok(UnixListener::bind(runtime.join("control")));
    std::thread::spawn(move || {
        let (mut s, _) = ok(listener.accept());
        let mut input = Vec::new();
        let mut got = Vec::new();
        let mut answers =
            std::iter::once(vec![Response::Welcome { version: VERSION }]).chain(answers);
        loop {
            if let Some((request, used)) = ok(control::decode_request(&input)) {
                input.drain(..used);
                if !matches!(request, Request::Hello { .. }) {
                    got.push(request);
                }
                let Some(batch) = answers.next() else { return got };
                let mut out = Vec::new();
                for answer in &batch {
                    ok(control::encode_response(answer, &mut out));
                }
                ok(s.write_all(&out));
            } else {
                let mut buf = [0u8; 4096];
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => return got,
                    Ok(n) => input.extend_from_slice(&buf[..n]),
                }
            }
        }
    })
}

/// `jackstay ARGS` against a daemon answering `answers`: its status, its stdout, and the requests
/// the daemon got.
fn exchange(args: &[&str], answers: Vec<Response>) -> (Option<i32>, String, Vec<Request>) {
    let root = dir();
    let server = scripted(&root, answers);
    let runtime = root.to_str().unwrap_or_default().to_owned();
    let mut full = vec!["--runtime-dir", &runtime];
    full.extend_from_slice(args);
    let out = jackstay(&full, false, None);
    let got = server.join().unwrap_or_default();
    (out.status.code(), String::from_utf8_lossy(&out.stdout).into_owned(), got)
}

#[test]
fn each_command_sends_its_request_and_ok_is_success() {
    for (args, want) in [
        (vec!["select", "7"], Request::Select { id: 7 }),
        (vec!["delete", "8"], Request::Delete { id: 8 }),
        (vec!["pin", "9"], Request::Pin { id: 9, pinned: true }),
        (vec!["unpin", "9"], Request::Pin { id: 9, pinned: false }),
        (vec!["clear"], Request::Clear),
        (vec!["clear-history"], Request::ClearHistory),
    ] {
        let (code, out, got) = exchange(&args, vec![Response::Ok]);
        assert_eq!((code, out.as_str(), got), (Some(0), "", vec![want]), "{args:?}");
    }
}

#[test]
fn refusals_are_1_and_say_why() {
    for (error, says) in [
        (Error::NoSuchEntry, "no such entry"),
        (Error::TooManyPinned, "too many pinned"),
        (Error::StoreFailed, "could not be updated"),
        (Error::ClaimFailed, "another application holds the clipboard"),
        (Error::ImportFailed, "could not be read or removed"),
        (Error::Internal, "jackstayd failed"),
    ] {
        let root = dir();
        let server = scripted(&root, vec![Response::Error(error)]);
        let out =
            jackstay(&["--runtime-dir", root.to_str().unwrap_or_default(), "clear"], false, None);
        let _ = server.join();
        assert_eq!(out.status.code(), Some(1), "{error:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains(says), "{error:?}");
    }
    // A response that does not fit the request is refused too.
    let (code, _, _) = exchange(&["clear"], vec![Response::Imported { added: 0, skipped: 0 }]);
    assert_eq!(code, Some(1));
}

#[test]
fn status_says_what_is_on_the_clipboard_and_warns_of_a_failing_store() {
    let status = |current, owns, failing| {
        Response::Status(Status {
            entries: 3,
            pinned: 1,
            current,
            owns_clipboard: owns,
            store_failing: failing,
        })
    };
    let (code, out, got) = exchange(&["status"], vec![status(Some(5), true, false)]);
    // The link is asked about too; this stand-in does not answer, and status says no more.
    assert_eq!((code, got), (Some(0), vec![Request::Status, Request::Link]));
    assert_eq!(out, "entries: 3 (1 pinned)\non the clipboard: entry 5\n");
    let (_, out, _) = exchange(&["status"], vec![status(None, true, true)]);
    assert_eq!(
        out,
        "entries: 3 (1 pinned)\non the clipboard: nothing (cleared)\n\
         warning: the history on disk is not up to date (see the journal)\n"
    );
    let (_, out, _) = exchange(&["status"], vec![status(None, false, false)]);
    assert!(out.ends_with("on the clipboard: not known\n"));
}

fn summary(id: u64, ago_s: u64, size: u64, kinds: u8) -> Summary {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0));
    Summary {
        id,
        copied_at_ms: now - ago_s * 1000 - 500,
        pinned: id == 2,
        current: id == 1,
        kinds: Kinds(kinds),
        size,
        source: Some("Fire\u{1b}fox".to_owned()),
        preview: format!("entry {id}"),
    }
}

#[test]
fn list_shows_each_entry_and_says_when_there_are_more() {
    let entries = vec![
        summary(1, 5, 10, Kinds::TEXT),
        summary(2, 90, 2048, Kinds::TEXT | Kinds::IMAGE),
        summary(3, 7200, 3 << 20, Kinds::FILES | Kinds::RICH_TEXT | Kinds::OTHER),
        summary(4, 3 * 86_400, 0, 0),
    ];
    let (code, out, got) = exchange(&["list"], vec![Response::List { total: 30, entries }]);
    assert_eq!((code, got), (Some(0), vec![Request::List { offset: 0, limit: 20 }]));
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 5, "{out}");
    let fields = |l: &str| l.split_whitespace().map(str::to_owned).collect::<Vec<_>>();
    // The source's escape is not passed to the terminal.
    assert_eq!(fields(lines[0]), ["1", "*", "5s", "10", "B", "text", "Fire", "fox", "entry", "1"]);
    assert_eq!(
        fields(lines[1]),
        ["2", "P", "1m", "2", "KiB", "text,image", "Fire", "fox", "entry", "2"]
    );
    assert_eq!(fields(lines[2])[1..5], ["2h", "3", "MiB", "files,rich,other"]);
    assert_eq!(fields(lines[3])[1..4], ["3d", "0", "B"]);
    assert_eq!(lines[4], "(4 of 30; jackstay list --all for every entry)");
    let (_, out, _) = exchange(
        &["list"],
        vec![Response::List { total: 1, entries: vec![summary(1, 0, 1, Kinds::TEXT)] }],
    );
    assert_eq!(out.lines().count(), 1, "no note when all are shown");
}

#[test]
fn list_all_pages_until_every_entry_is_shown() {
    let page =
        |from: u64, n: u64| (from..from + n).map(|i| summary(i + 10, 1, 1, Kinds::TEXT)).collect();
    let (code, out, got) = exchange(
        &["list", "--all"],
        vec![
            Response::List { total: 700, entries: page(0, 500) },
            Response::List { total: 700, entries: page(500, 200) },
        ],
    );
    assert_eq!(code, Some(0));
    assert_eq!(
        got,
        [Request::List { offset: 0, limit: 500 }, Request::List { offset: 500, limit: 500 }]
    );
    assert_eq!(out.lines().count(), 700);
    // An empty page ends it, whatever the total says.
    let (_, _, got) = exchange(
        &["list", "--all"],
        vec![
            Response::List { total: 900, entries: page(0, 500) },
            Response::List { total: 900, entries: vec![] },
        ],
    );
    assert_eq!(got.len(), 2);
    // An error mid-way is a refusal.
    let (code, _, _) = exchange(
        &["list", "--all"],
        vec![
            Response::List { total: 900, entries: page(0, 500) },
            Response::Error(Error::Internal),
        ],
    );
    assert_eq!(code, Some(1));
}

#[test]
fn import_reports_counts_and_where_clipmans_files_are() {
    let (code, out, got) =
        exchange(&["import-clipman"], vec![Response::Imported { added: 3, skipped: 1 }]);
    assert_eq!((code, got), (Some(0), vec![Request::ImportClipman { remove: false }]));
    assert_eq!(
        out,
        "imported 3 entries from clipman (1 skipped)\n\
         clipman's own files still hold its history: jackstay import-clipman --remove\n"
    );
    let (_, out, got) = exchange(
        &["import-clipman", "--remove"],
        vec![Response::Imported { added: 3, skipped: 0 }],
    );
    assert_eq!(got, [Request::ImportClipman { remove: true }]);
    assert_eq!(out, "imported 3 entries from clipman (0 skipped)\n");
    let (_, out, _) =
        exchange(&["import-clipman"], vec![Response::Imported { added: 0, skipped: 2 }]);
    assert_eq!(out.lines().count(), 1, "nothing added, nothing to remove");
    assert_eq!(jackstay(&["import-clipman", "--all"], false, None).status.code(), Some(2));
    assert_eq!(jackstay(&["list", "--remove"], false, None).status.code(), Some(2));
}

fn link(peers: Vec<PeerSummary>) -> LinkStatus {
    LinkStatus {
        port: Some(24810),
        fingerprint: "0123456789abcdef".into(),
        peers,
        ..LinkStatus::default()
    }
}

fn xiphos(state: PeerState, rtt_us: Option<u32>) -> PeerSummary {
    PeerSummary {
        name: "xiphos\u{1b}[2J".into(),
        fingerprint: "fedcba9876543210".into(),
        address: "xiphos".into(),
        state,
        rtt_us,
        connects: 2,
    }
}

#[test]
fn status_and_peers_show_the_link() {
    let answers = vec![
        Response::Status(Status {
            entries: 0,
            pinned: 0,
            current: None,
            owns_clipboard: false,
            store_failing: false,
        }),
        Response::Link(link(vec![xiphos(PeerState::Connected, Some(1234))])),
    ];
    let (code, out, _) = exchange(&["status"], answers);
    assert_eq!(code, Some(0));
    assert!(out.contains("link: UDP port 24810, this machine 0123456789abcdef\n"), "{out}");
    // The peer's name is cleaned of the escape it sent.
    assert!(out.contains("  xiphos [2J connected, 1.234 ms\n"), "{out}");
    let (code, out, got) = exchange(
        &["peers"],
        vec![Response::Link(link(vec![xiphos(PeerState::RefusedVersion, None)]))],
    );
    assert_eq!((code, got), (Some(0), vec![Request::Link]));
    assert!(out.contains("refused: it runs another version\n"), "{out}");
    assert!(out.contains("fingerprint fedcba9876543210, address xiphos, sessions 2"), "{out}");
    let (_, out, _) = exchange(&["peers"], vec![Response::Link(link(Vec::new()))]);
    assert!(out.contains("no paired machines (jackstay pair HOST)"), "{out}");
    let off = LinkStatus { off: Some("disabled".into()), ..LinkStatus::default() };
    let (_, out, _) = exchange(&["peers"], vec![Response::Link(off)]);
    assert_eq!(out, "link: off (disabled)\n");
}

/// `jackstay pair HOST` answering `input`, against a daemon whose answer to the code is
/// `outcome`: its status, stdout, stderr, and the requests the daemon got.
fn pairing(input: &str, outcome: Response) -> (Option<i32>, String, String, Vec<Request>) {
    let root = dir();
    let code = Response::PairCode {
        name: "xiphos".into(),
        fingerprint: "fedcba9876543210".into(),
        code: "123456".into(),
    };
    let server =
        scripted_batches(&root, vec![vec![Response::Ok, code], vec![Response::Ok, outcome]]);
    let runtime = root.to_str().unwrap_or_default().to_owned();
    let out = jackstay_with_input(
        &["--runtime-dir", &runtime, "pair", "xiphos"],
        false,
        None,
        Some(input),
    );
    let got = server.join().unwrap_or_default();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        got,
    )
}

#[test]
fn pair_shows_the_code_and_sends_the_answer() {
    let paired = Response::Paired { name: "xiphos".into(), fingerprint: "fedcba9876543210".into() };
    let (code, out, _, got) = pairing("y\n", paired);
    assert_eq!(code, Some(0));
    assert_eq!(
        got,
        vec![Request::Pair { host: "xiphos".into() }, Request::PairAnswer { yes: true }]
    );
    assert!(out.contains("xiphos (fingerprint fedcba9876543210) shows the code 123 456."), "{out}");
    assert!(out.contains("Paired with xiphos (fedcba9876543210)."), "{out}");
}

/// Reported: `jackstay pair` sat silent until the other machine started pairing too.
#[test]
fn pair_says_it_is_waiting_for_the_other_machine_before_the_code() {
    let paired = Response::Paired { name: "xiphos".into(), fingerprint: "fedcba9876543210".into() };
    let (_, out, _, _) = pairing("y\n", paired);
    let first = out.lines().next().unwrap_or_default();
    assert!(first.starts_with("Waiting for xiphos to start pairing too"), "{out}");
    assert!(first.contains("jackstay pair "), "{out}");
    assert_eq!(out.matches("start pairing too").count(), 1, "{out}");
}

#[test]
fn pair_refused_at_once_does_not_say_it_is_waiting() {
    let busy = Response::PairFailed { reason: PairReason::Busy, detail: String::new() };
    let root = dir();
    let server = scripted_batches(&root, vec![vec![busy]]);
    let runtime = root.to_str().unwrap_or_default().to_owned();
    let out =
        jackstay_with_input(&["--runtime-dir", &runtime, "pair", "xiphos"], false, None, Some(""));
    let _ = server.join();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1));
    assert!(!stdout.contains("Waiting"), "{stdout}");
}

#[test]
fn pair_answers_no_unless_told_yes() {
    for input in ["n\n", "\n", "", "yes please\n"] {
        let declined = Response::PairFailed { reason: PairReason::Declined, detail: String::new() };
        let (code, _, err, got) = pairing(input, declined);
        assert_eq!(code, Some(1), "{input:?}");
        assert_eq!(got.last(), Some(&Request::PairAnswer { yes: false }), "{input:?}");
        assert!(err.contains("pairing failed: declined here"), "{err}");
    }
}

#[test]
fn unpair_names_the_machine_and_says_why_it_cannot() {
    let (code, _, got) = exchange(&["unpair", "xiphos"], vec![Response::Ok]);
    assert_eq!((code, got), (Some(0), vec![Request::Unpair { peer: "xiphos".into() }]));
    let root = dir();
    let server = scripted(&root, vec![Response::Error(Error::AmbiguousPeer)]);
    let out =
        jackstay(&["--runtime-dir", root.to_str().unwrap_or_default(), "unpair", "x"], false, None);
    let _ = server.join();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("give its fingerprint"));
}
