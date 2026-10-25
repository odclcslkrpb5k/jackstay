//! The command line's exit statuses, and `popup` until the popup exists, against a stand-in
//! daemon.

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use jackstay_proto::control::{self, Error, Kinds, Request, Response, Status, Summary, VERSION};

#[track_caller]
fn ok<T, E: std::fmt::Debug>(r: Result<T, E>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("{e:?}"),
    }
}

fn dir(name: &str) -> PathBuf {
    let d =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("cli-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    ok(std::fs::create_dir_all(&d));
    d
}

/// `jackstay` with `args`, no display unless `display`, and `bin` first on its `PATH`.
fn jackstay(args: &[&str], display: bool, bin: Option<&Path>) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_jackstay"));
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
    ok(c.output())
}

#[test]
fn bad_usage_is_2_and_no_daemon_is_3() {
    assert_eq!(jackstay(&[], false, None).status.code(), Some(2));
    assert_eq!(jackstay(&["select", "x"], false, None).status.code(), Some(2));
    let empty = dir("empty");
    let runtime = empty.to_str().unwrap_or_default();
    assert_eq!(jackstay(&["--runtime-dir", runtime, "status"], false, None).status.code(), Some(3));
    let help = jackstay(&["help"], false, None);
    assert!(String::from_utf8_lossy(&help.stdout).contains("popup"));
}

/// `popup` and `settings` ask the running jackstay-ui; without one they fail with status 3, and say
/// so as a desktop notification too, since run from a hotkey or a menu nothing shows stderr.
#[test]
fn popup_and_settings_ask_the_ui_and_say_so_where_a_user_sees_it_if_none_runs() {
    let root = dir("popup");
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
    let listener = ok(UnixListener::bind(runtime.join("control")));
    std::thread::spawn(move || {
        let (mut s, _) = ok(listener.accept());
        let mut input = Vec::new();
        let mut got = Vec::new();
        let mut answers = std::iter::once(Response::Welcome { version: VERSION }).chain(answers);
        loop {
            if let Some((request, used)) = ok(control::decode_request(&input)) {
                input.drain(..used);
                if !matches!(request, Request::Hello { .. }) {
                    got.push(request);
                }
                let Some(answer) = answers.next() else { return got };
                let mut out = Vec::new();
                ok(control::encode_response(&answer, &mut out));
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
fn exchange(
    name: &str,
    args: &[&str],
    answers: Vec<Response>,
) -> (Option<i32>, String, Vec<Request>) {
    let root = dir(name);
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
        let (code, out, got) = exchange("each", &args, vec![Response::Ok]);
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
        let root = dir("refused");
        let server = scripted(&root, vec![Response::Error(error)]);
        let out =
            jackstay(&["--runtime-dir", root.to_str().unwrap_or_default(), "clear"], false, None);
        let _ = server.join();
        assert_eq!(out.status.code(), Some(1), "{error:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains(says), "{error:?}");
    }
    // A response that does not fit the request is refused too.
    let (code, _, _) =
        exchange("mismatch", &["clear"], vec![Response::Imported { added: 0, skipped: 0 }]);
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
    let (code, out, got) = exchange("status1", &["status"], vec![status(Some(5), true, false)]);
    assert_eq!((code, got), (Some(0), vec![Request::Status]));
    assert_eq!(out, "entries: 3 (1 pinned)\non the clipboard: entry 5\n");
    let (_, out, _) = exchange("status2", &["status"], vec![status(None, true, true)]);
    assert_eq!(
        out,
        "entries: 3 (1 pinned)\non the clipboard: nothing (cleared)\n\
         warning: the history on disk is not up to date (see the journal)\n"
    );
    let (_, out, _) = exchange("status3", &["status"], vec![status(None, false, false)]);
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
    let (code, out, got) = exchange("list", &["list"], vec![Response::List { total: 30, entries }]);
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
        "list-all-shown",
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
        "all",
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
        "all-empty",
        &["list", "--all"],
        vec![
            Response::List { total: 900, entries: page(0, 500) },
            Response::List { total: 900, entries: vec![] },
        ],
    );
    assert_eq!(got.len(), 2);
    // An error mid-way is a refusal.
    let (code, _, _) = exchange(
        "all-error",
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
        exchange("import", &["import-clipman"], vec![Response::Imported { added: 3, skipped: 1 }]);
    assert_eq!((code, got), (Some(0), vec![Request::ImportClipman { remove: false }]));
    assert_eq!(
        out,
        "imported 3 entries from clipman (1 skipped)\n\
         clipman's own files still hold its history: jackstay import-clipman --remove\n"
    );
    let (_, out, got) = exchange(
        "import-remove",
        &["import-clipman", "--remove"],
        vec![Response::Imported { added: 3, skipped: 0 }],
    );
    assert_eq!(got, [Request::ImportClipman { remove: true }]);
    assert_eq!(out, "imported 3 entries from clipman (0 skipped)\n");
    let (_, out, _) = exchange(
        "import-none",
        &["import-clipman"],
        vec![Response::Imported { added: 0, skipped: 2 }],
    );
    assert_eq!(out.lines().count(), 1, "nothing added, nothing to remove");
    assert_eq!(jackstay(&["import-clipman", "--all"], false, None).status.code(), Some(2));
    assert_eq!(jackstay(&["list", "--remove"], false, None).status.code(), Some(2));
}
