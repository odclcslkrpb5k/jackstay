//! The blocking client against a stand-in daemon that answers the greeting as told.

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use jackstay_proto::control::{self, Error, Response, VERSION};
use jackstayd::client::{Client, ClientError};

fn ok<T, E: std::fmt::Debug>(r: Result<T, E>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("{e:?}"),
    }
}

/// A daemon that reads one request and sends `answers`, then waits for the client to go.
fn daemon(name: &str, answers: Vec<Response>) -> (PathBuf, std::thread::JoinHandle<()>) {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("client-{}-{name}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let listener = ok(UnixListener::bind(&path));
    let thread = std::thread::spawn(move || {
        let (mut s, _) = ok(listener.accept());
        let mut buf = [0u8; 64];
        let _ = s.read(&mut buf);
        let mut out = Vec::new();
        for a in &answers {
            ok(control::encode_response(a, &mut out));
        }
        ok(s.write_all(&out));
        // With nothing to say, it hangs up at once.
        if !answers.is_empty() {
            let _ = s.read(&mut buf);
        }
    });
    (path, thread)
}

fn connect(name: &str, answers: Vec<Response>) -> Result<Client, ClientError> {
    let (path, thread) = daemon(name, answers);
    let client = Client::connect(&path, Duration::from_secs(5));
    if client.is_err() {
        let _ = thread.join();
    }
    client
}

#[test]
fn the_greeting_decides_the_connection() {
    assert!(connect("welcome", vec![Response::Welcome { version: VERSION }]).is_ok());
    assert!(matches!(
        connect("other", vec![Response::Welcome { version: VERSION + 1 }]),
        Err(ClientError::Refused)
    ));
    assert!(matches!(
        connect("old", vec![Response::Error(Error::UnsupportedVersion)]),
        Err(ClientError::Version)
    ));
    assert!(matches!(
        connect("sequence", vec![Response::Error(Error::BadSequence)]),
        Err(ClientError::Refused)
    ));
    assert!(matches!(connect("gone", vec![]), Err(ClientError::Closed)));
}

#[test]
fn events_are_not_taken_for_responses() {
    let (path, _thread) = daemon(
        "events",
        vec![Response::Changed, Response::Changed, Response::Welcome { version: VERSION }],
    );
    assert!(Client::connect(&path, Duration::from_secs(5)).is_ok());
}

#[test]
fn errors_say_what_failed() {
    assert_eq!(ClientError::Closed.to_string(), "jackstayd closed the connection");
    assert_eq!(ClientError::Version.to_string(), "jackstayd speaks another protocol version");
}
