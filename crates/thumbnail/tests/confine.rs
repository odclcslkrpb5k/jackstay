//! What a confined process can no longer do (docs/daemon.md, "Thumbnails"). Each probe runs in a
//! child: this test binary again, which confines itself, tries one thing, and must be killed for
//! it, as a decoder made to run an attacker's code would be. The control probe only reads and
//! writes its standard streams, and must live.

use std::io::{Read as _, Write as _};
use std::os::unix::process::ExitStatusExt as _;
use std::process::{Command, Stdio};

const PROBE: &str = "JACKSTAY_CONFINE_PROBE";
/// SIGSYS: what seccomp's kill action ends a process with.
const SIGSYS: i32 = 31;

fn probe(kind: &str) {
    // Made before confining: an inherited descriptor past the standard three.
    let (_r, w) = std::io::pipe().unwrap_or_else(|e| panic!("{e}"));
    if jackstay_thumbnail::confine().is_err() {
        std::process::exit(3);
    }
    match kind {
        "streams" => {
            let mut input = Vec::new();
            let _ = std::io::stdin().read_to_end(&mut input);
            let _ = std::io::stdout().write_all(&input);
            // Memory still works.
            let big = vec![7u8; 64 << 20];
            std::process::exit(i32::from(big.iter().any(|&b| b != 7)));
        }
        "open" => drop(std::fs::File::open("/etc/hostname")),
        "create" => drop(std::fs::File::create("/tmp/jackstay-confine-probe")),
        "unlink" => drop(std::fs::remove_file("/tmp/jackstay-confine-probe-none")),
        "signal" => drop(rustix::process::kill_process(
            rustix::process::getppid().unwrap_or_else(|| panic!("no parent")),
            rustix::process::Signal::CONT,
        )),
        "inherited" => drop((&w).write_all(b"x")),
        "spawn" => drop(Command::new("/bin/true").status()),
        "thread" => drop(std::thread::spawn(|| ()).join()),
        "socket" => drop(std::os::unix::net::UnixStream::connect("/tmp/.X11-unix/X0")),
        other => panic!("no probe {other}"),
    }
    // Survived.
    std::process::exit(4);
}

fn run(kind: &str) -> std::process::ExitStatus {
    let me = std::env::current_exe().unwrap_or_else(|e| panic!("{e}"));
    let mut child = Command::new(me)
        .args(["--exact", "probes", "--nocapture", "--test-threads", "1"])
        .env(PROBE, kind)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("{e}"));
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"in");
    }
    child.wait().unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn probes() {
    if let Ok(kind) = std::env::var(PROBE) {
        probe(&kind);
    }
    let control = run("streams");
    assert_eq!(control.code(), Some(0), "the streams and memory still work: {control:?}");
    for kind in ["open", "create", "unlink", "signal", "inherited", "spawn", "thread", "socket"] {
        let status = run(kind);
        assert_eq!(status.signal(), Some(SIGSYS), "{kind}: {status:?}");
    }
}
