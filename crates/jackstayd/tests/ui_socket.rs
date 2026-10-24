//! The client side of the UI's socket, against a stand-in UI.

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use jackstayd::ui_socket::{self, request_popup};

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
        let mut got = [0u8; 6];
        ok(s.read_exact(&mut got));
        ok(s.write_all(answer));
        got.to_vec()
    })
}

#[test]
fn the_popup_is_asked_for_on_the_ui_socket_and_ok_is_awaited() {
    let dir = runtime("ok");
    assert_eq!(ui_socket::path(&dir), dir.join("ui"));
    let server = ui(&dir, b"ok\n");
    ok(request_popup(&dir, Duration::from_secs(5)));
    assert_eq!(ok(server.join().map_err(|_| "ui")), b"popup\n");
    let dir = runtime("other");
    let server = ui(&dir, b"no\n");
    let e = request_popup(&dir, Duration::from_secs(5)).err().map(|e| e.kind());
    assert_eq!(e, Some(std::io::ErrorKind::InvalidData));
    let _ = server.join();
    let dir = runtime("none");
    assert!(request_popup(&dir, Duration::from_secs(1)).is_err());
}
