//! The UI's socket, served without a display.

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use jackstay_ui::listen::{ListenError, bind};
use jackstayd::ui_socket::{self, Ask};

#[track_caller]
fn ok<T, E: std::fmt::Debug>(r: Result<T, E>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("{e:?}"),
    }
}

fn runtime(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("listen-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Sends `bytes`, closes its end, and returns what came back.
fn ask(runtime: &Path, bytes: &[u8]) -> Vec<u8> {
    let mut s = ok(UnixStream::connect(ui_socket::path(runtime)));
    ok(s.set_read_timeout(Some(Duration::from_secs(5))));
    ok(s.write_all(bytes));
    ok(s.shutdown(std::net::Shutdown::Write));
    let mut back = Vec::new();
    let _ = s.read_to_end(&mut back);
    back
}

#[test]
fn only_a_whole_request_is_answered() {
    let dir = runtime("requests");
    let listener = ok(bind(&dir));
    let asked = Arc::new(AtomicUsize::new(0));
    let settings = Arc::new(AtomicUsize::new(0));
    let (count, count_settings) = (Arc::clone(&asked), Arc::clone(&settings));
    ok(listener.serve(move |ask| {
        let n = if ask == Ask::Settings { &count_settings } else { &count };
        n.fetch_add(1, Ordering::SeqCst);
    }));
    assert_eq!(ask(&dir, b"popup\n"), ui_socket::OK);
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    assert_eq!(ask(&dir, b"settings\n"), ui_socket::OK);
    assert_eq!(settings.load(Ordering::SeqCst), 1);
    for junk in [&b"popu"[..], b"popup", b"POPUP\n", b"", b"quit!!", b"settings!\n", b"popup!\n"] {
        assert_eq!(ask(&dir, junk), b"", "{junk:?}");
    }
    // A client that says nothing is given up on, and the next is served.
    let silent = ok(UnixStream::connect(ui_socket::path(&dir)));
    let began = Instant::now();
    assert_eq!(ask(&dir, b"popup\n"), ui_socket::OK);
    assert!(began.elapsed() < Duration::from_secs(2));
    drop(silent);
    assert_eq!(asked.load(Ordering::SeqCst), 2);
    // Through the client the CLI uses.
    ok(ui_socket::request(&dir, Ask::Popup, Duration::from_secs(5)));
    assert_eq!(asked.load(Ordering::SeqCst), 3);
    assert_eq!(settings.load(Ordering::SeqCst), 1);
}

/// Half a second for the whole request, not for each byte of it.
#[test]
fn a_client_sending_slowly_is_given_up_on() {
    let dir = runtime("slow");
    let listener = ok(bind(&dir));
    let asked = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&asked);
    ok(listener.serve(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
    }));
    let mut s = ok(UnixStream::connect(ui_socket::path(&dir)));
    ok(s.set_read_timeout(Some(Duration::from_secs(5))));
    for b in b"settings\n" {
        let _ = s.write_all(&[*b]);
        std::thread::sleep(Duration::from_millis(100));
    }
    let mut back = Vec::new();
    let _ = s.read_to_end(&mut back);
    assert_eq!(back, b"");
    assert_eq!(asked.load(Ordering::SeqCst), 0);
}

#[test]
fn one_ui_at_a_time_and_a_stale_socket_is_replaced() {
    let dir = runtime("lock");
    ok(std::fs::create_dir_all(&dir));
    ok(std::fs::write(ui_socket::path(&dir), b"left by a crash"));
    let first = ok(bind(&dir));
    assert!(matches!(bind(&dir), Err(ListenError::Running)));
    drop(first);
    assert!(bind(&dir).is_ok(), "free again once the first is gone");
}
