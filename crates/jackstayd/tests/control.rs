//! The control server's bounds, driven in-process: each turn of the session loop is played by
//! hand, so what the server does at each step can be seen.

use std::io::{Read as _, Write as _};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use jackstay_proto::control::{
    self, Error, Kinds, MAX_LIST, MAX_PREVIEW_CHARS, Request, Response, Summary, VERSION,
};
use jackstayd::control::{ClientId, Clients, Dropped, MAX_CLIENTS, MAX_OUTPUT, REQUESTS_PER_TURN};

#[track_caller]
fn ok<T, E: std::fmt::Debug>(r: Result<T, E>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("{e:?}"),
    }
}

fn bind(name: &str) -> (Clients, PathBuf) {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("control-{}-{name}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    (ok(Clients::bind(&path)), path)
}

fn connect(path: &Path) -> UnixStream {
    let s = ok(UnixStream::connect(path));
    ok(s.set_read_timeout(Some(Duration::from_secs(5))));
    s
}

fn send(s: &mut UnixStream, requests: &[Request]) {
    let mut out = Vec::new();
    for r in requests {
        control::encode_request(r, &mut out);
    }
    ok(s.write_all(&out));
}

const HELLO: Request = Request::Hello { version: VERSION };

/// Every response until the server closes the connection.
fn until_closed(s: &mut UnixStream) -> Vec<Response> {
    let mut bytes = Vec::new();
    ok(s.read_to_end(&mut bytes));
    decode_all(&bytes)
}

/// The next `n` responses.
fn read_n(s: &mut UnixStream, n: usize) -> Vec<Response> {
    let mut bytes = Vec::new();
    loop {
        let got = decode_all(&bytes);
        if got.len() >= n {
            return got;
        }
        let mut buf = [0u8; 4096];
        let k = ok(s.read(&mut buf));
        assert!(k > 0, "closed after {got:?}");
        bytes.extend_from_slice(buf.get(..k).unwrap_or_default());
    }
}

fn decode_all(mut bytes: &[u8]) -> Vec<Response> {
    let mut got = Vec::new();
    while let Some((r, used)) = ok(control::decode_response(bytes)) {
        got.push(r);
        bytes = bytes.get(used..).unwrap_or_default();
    }
    got
}

fn ids(c: &Clients) -> Vec<ClientId> {
    c.client_fds().map(|(id, ..)| id).collect()
}

/// Whether the server would poll `id` for reading.
fn reading(c: &Clients, id: ClientId) -> bool {
    c.client_fds().any(|(i, _, r, _)| i == id && r)
}

/// Reads every client and takes each one's requests, as far as the server allows.
fn requests(c: &mut Clients) -> Vec<(ClientId, Request)> {
    let mut got = Vec::new();
    for id in ids(c) {
        let _ = c.read(id);
        while let Ok(Some(r)) = c.next(id) {
            got.push((id, r));
        }
    }
    got
}

/// One accepted client, its connection, and the server.
fn one(name: &str) -> (Clients, UnixStream, ClientId) {
    let (mut c, path) = bind(name);
    let s = connect(&path);
    assert_eq!(c.accept(), []);
    let id = ids(&c).first().copied().unwrap_or_else(|| panic!("not accepted"));
    (c, s, id)
}

#[test]
fn the_greeting_is_checked_and_only_greeted_clients_stay() {
    let (mut c, path) = bind("greeting");
    let mut other_version = connect(&path);
    let mut no_hello = connect(&path);
    let mut twice = connect(&path);
    let mut good = connect(&path);
    assert_eq!(c.accept(), []);
    assert!(c.listener_fd().is_some(), "nothing waiting is not a failure");
    assert_eq!((c.len(), c.is_empty()), (4, false));
    send(&mut other_version, &[Request::Hello { version: VERSION + 1 }, Request::Status]);
    send(&mut no_hello, &[Request::Status]);
    send(&mut twice, &[HELLO, HELLO]);
    send(&mut good, &[HELLO, Request::Subscribe, Request::Status]);
    let handed = requests(&mut c);
    assert_eq!(handed.iter().map(|(_, r)| r).collect::<Vec<_>>(), [&Request::Status]);
    let (good_id, _) = handed.first().copied().unwrap_or_else(|| panic!("none"));
    c.respond(good_id, &Response::Ok);
    c.changed();
    // Those that broke the protocol are closed once answered, and not reported as dropped.
    assert_eq!(c.flush(), []);
    assert_eq!(until_closed(&mut other_version), [Response::Error(Error::UnsupportedVersion)]);
    assert_eq!(until_closed(&mut no_hello), [Response::Error(Error::BadSequence)]);
    assert_eq!(
        until_closed(&mut twice),
        [Response::Welcome { version: VERSION }, Response::Error(Error::BadSequence)]
    );
    assert_eq!(
        read_n(&mut good, 4),
        [Response::Welcome { version: VERSION }, Response::Ok, Response::Ok, Response::Changed]
    );
    assert_eq!(ids(&c), [good_id], "the greeted client stays");
    drop(good);
    let _ = requests(&mut c);
    assert_eq!(c.flush(), []);
    assert_eq!((c.len(), c.is_empty()), (0, true));
}

#[test]
fn clients_past_the_limit_are_closed_at_once() {
    let (mut c, path) = bind("limit");
    let kept: Vec<UnixStream> = (0..MAX_CLIENTS).map(|_| connect(&path)).collect();
    let mut extra = connect(&path);
    assert_eq!(c.accept(), [Dropped::TooManyClients]);
    assert_eq!(c.len(), MAX_CLIENTS);
    assert_eq!(until_closed(&mut extra), []);
    drop(kept);
}

#[test]
fn a_client_gets_a_bounded_number_of_requests_per_turn() {
    let (mut c, mut s, id) = one("budget");
    let mut batch = vec![HELLO];
    batch.extend(std::iter::repeat_n(Request::Status, 100));
    send(&mut s, &batch);
    // Wait until all of it is buffered, so the counts below do not depend on timing.
    std::thread::sleep(Duration::from_millis(50));
    c.begin_turn();
    assert_eq!(requests(&mut c).len(), REQUESTS_PER_TURN - 1, "Hello counts");
    assert_eq!(c.ready(), [], "the rest waits for the next turn");
    c.begin_turn();
    assert_eq!(c.ready(), [id]);
    assert_eq!(requests(&mut c).len(), 100 - (REQUESTS_PER_TURN - 1));
    assert_eq!(c.ready(), []);
}

#[test]
fn a_deferred_request_holds_back_the_next_until_resolved() {
    let (mut c, mut s, id) = one("defer");
    send(&mut s, &[HELLO, Request::Clear, Request::Status]);
    std::thread::sleep(Duration::from_millis(50));
    let _ = c.read(id);
    assert_eq!(ok(c.next(id)), Some(Request::Clear));
    c.defer(id);
    assert_eq!(ok(c.next(id)), None);
    assert!(!reading(&c, id), "not read while busy");
    assert_eq!(c.ready(), []);
    c.resolve(id, &Response::Ok);
    assert!(reading(&c, id));
    assert_eq!(c.ready(), [id]);
    assert_eq!(ok(c.next(id)), Some(Request::Status));
    // Resolved with nothing left to do, it stays connected for more.
    c.defer(id);
    c.resolve(id, &Response::Ok);
    assert_eq!(c.flush(), []);
    assert_eq!(c.len(), 1);
    send(&mut s, &[Request::Status]);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(requests(&mut c), [(id, Request::Status)]);
    assert_eq!(
        read_n(&mut s, 3),
        [Response::Welcome { version: VERSION }, Response::Ok, Response::Ok]
    );
}

#[test]
fn a_half_closed_client_is_answered_before_it_is_closed() {
    let (mut c, mut s, id) = one("half");
    send(&mut s, &[HELLO, Request::Clear]);
    ok(s.shutdown(Shutdown::Write));
    std::thread::sleep(Duration::from_millis(50));
    let _ = c.read(id);
    assert_eq!(ok(c.next(id)), Some(Request::Clear));
    c.defer(id);
    let _ = c.read(id);
    assert_eq!(c.flush(), []);
    assert_eq!(c.len(), 1, "still owed a response");
    c.resolve(id, &Response::Ok);
    assert_eq!(c.flush(), []);
    assert_eq!(c.len(), 0);
    assert_eq!(until_closed(&mut s), [Response::Welcome { version: VERSION }, Response::Ok]);

    // One with nothing owed is closed once what it sent is handled.
    let (mut c, mut s, id) = one("half-idle");
    send(&mut s, &[HELLO]);
    std::thread::sleep(Duration::from_millis(50));
    let _ = c.read(id);
    assert_eq!(ok(c.next(id)), None);
    ok(s.shutdown(Shutdown::Write));
    std::thread::sleep(Duration::from_millis(50));
    let _ = c.read(id);
    assert_eq!(c.flush(), []);
    assert_eq!(c.len(), 0);
    assert_eq!(until_closed(&mut s), [Response::Welcome { version: VERSION }]);
}

fn big_list() -> Response {
    let entry = |id| Summary {
        id,
        copied_at_ms: 0,
        pinned: false,
        current: false,
        kinds: Kinds(Kinds::TEXT),
        size: 1,
        source: None,
        preview: "x".repeat(MAX_PREVIEW_CHARS),
    };
    Response::List { total: 0, entries: (0..).take(MAX_LIST).map(entry).collect() }
}

#[test]
fn a_client_that_does_not_read_is_dropped_and_others_are_not() {
    let (mut c, mut s, id) = one("slow");
    send(&mut s, &[HELLO, Request::Status]);
    std::thread::sleep(Duration::from_millis(50));
    let _ = c.read(id);
    assert_eq!(ok(c.next(id)), Some(Request::Status));
    let mut size = Vec::new();
    ok(control::encode_response(&big_list(), &mut size));
    for _ in 0..(MAX_OUTPUT * 2 / size.len()) {
        c.respond(id, &big_list());
    }
    assert!(!reading(&c, id), "past the high-water mark, not read");
    assert_eq!(c.flush(), [Dropped::TooSlow]);
    assert_eq!(c.len(), 0);
}

#[test]
fn a_reset_connection_is_dropped() {
    let (mut c, mut s, id) = one("reset");
    send(&mut s, &[HELLO]);
    std::thread::sleep(Duration::from_millis(50));
    let _ = c.read(id);
    assert_eq!(ok(c.next(id)), None);
    assert_eq!(c.flush(), []);
    // Closed with the Welcome unread: Linux reports that to the server as a reset.
    std::thread::sleep(Duration::from_millis(50));
    drop(s);
    assert_eq!(c.read(id), Err(Dropped::Closed));
    assert_eq!(c.len(), 0);
}
