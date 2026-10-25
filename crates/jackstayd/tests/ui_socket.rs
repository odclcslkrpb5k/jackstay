//! The client side of the UI's socket, against a stand-in UI.

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use jackstayd::ui_socket::{self, Ask, request};

#[track_caller]
fn ok<T, E: std::fmt::Debug>(r: Result<T, E>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("{e:?}"),
    }
}

fn runtime(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("uisock-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    ok(std::fs::create_dir_all(&d));
    d
}

/// A UI that reads the request and answers `answer`; returns what it was sent.
fn ui(runtime: &Path, answer: &'static [u8]) -> std::thread::JoinHandle<Vec<u8>> {
    let listener = ok(UnixListener::bind(runtime.join("ui")));
    std::thread::spawn(move || {
        let (mut s, _) = ok(listener.accept());
        let mut got = Vec::new();
        let mut byte = [0u8];
        while got.last() != Some(&b'\n') {
            ok(s.read_exact(&mut byte));
            got.push(byte[0]);
        }
        ok(s.write_all(answer));
        got
    })
}

#[test]
fn a_window_is_asked_for_on_the_ui_socket_and_ok_is_awaited() {
    let dir = runtime("ok");
    assert_eq!(ui_socket::path(&dir), dir.join("ui"));
    let server = ui(&dir, b"ok\n");
    ok(request(&dir, Ask::Popup, Duration::from_secs(5)));
    assert_eq!(ok(server.join().map_err(|_| "ui")), b"popup\n");
    let dir = runtime("settings");
    let server = ui(&dir, b"ok\n");
    ok(request(&dir, Ask::Settings, Duration::from_secs(5)));
    assert_eq!(ok(server.join().map_err(|_| "ui")), b"settings\n");
    let dir = runtime("other");
    let server = ui(&dir, b"no\n");
    let e = request(&dir, Ask::Popup, Duration::from_secs(5)).err().map(|e| e.kind());
    assert_eq!(e, Some(std::io::ErrorKind::InvalidData));
    let _ = server.join();
    let dir = runtime("none");
    assert!(request(&dir, Ask::Popup, Duration::from_secs(1)).is_err());
}

#[test]
fn a_request_is_one_whole_line() {
    for ask in [Ask::Popup, Ask::Settings] {
        assert_eq!(Ask::parse(ask.line()), Some(ask));
        assert!(ask.line().len() <= Ask::LONGEST);
    }
    assert_eq!(Ask::LONGEST, Ask::Settings.line().len(), "no longer than it must be");
    for junk in [&b"popup"[..], b"settings", b"Popup\n", b"popup\n\n", b"", b"\n"] {
        assert_eq!(Ask::parse(junk), None, "{junk:?}");
    }
}
