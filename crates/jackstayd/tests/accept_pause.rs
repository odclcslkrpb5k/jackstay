//! Out of descriptors, the listener pauses instead of spinning on a connection it cannot take. In
//! a binary of its own: it lowers this process's descriptor limit.

use std::fs::File;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use jackstayd::control::{Clients, Dropped};
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

fn ok<T, E: std::fmt::Debug>(r: Result<T, E>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("{e:?}"),
    }
}

#[test]
fn accepting_pauses_while_descriptors_run_out() {
    let path =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("accept-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut clients = ok(Clients::bind(&path));
    let _waiting = ok(UnixStream::connect(&path));
    let limit = getrlimit(Resource::Nofile);
    ok(setrlimit(Resource::Nofile, Rlimit { current: Some(64), maximum: limit.maximum }));
    let mut filler = Vec::new();
    while let Ok(f) = File::open("/dev/null") {
        filler.push(f);
    }
    let failed = clients.accept();
    let paused = clients.listener_fd().is_none();
    drop(filler);
    ok(setrlimit(Resource::Nofile, limit));
    assert_eq!(failed, [Dropped::Failed]);
    assert!(paused, "not polled while paused");
    std::thread::sleep(Duration::from_millis(1_100));
    assert!(clients.listener_fd().is_some(), "polled again after the pause");
    assert_eq!(clients.accept(), []);
    assert_eq!(clients.len(), 1, "the waiting client taken once descriptors are free");
}
