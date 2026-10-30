//! Refusing to run beside xfce4-clipman (docs/daemon.md, "Coexistence"): two managers fight over
//! the clipboard. clipman's own claim on `CLIPBOARD_MANAGER` is seen by the engine; these find it
//! when it runs without one.

use std::fs;
use std::io::Read as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// How long `xfconf-query` may take before it is given up on.
const XFCONF_TIMEOUT: Duration = Duration::from_secs(2);

/// The panel plugin's id, as xfconf lists it and as the panel's wrapper is given it.
const PLUGIN_ID: &str = "xfce4-clipman-plugin";

/// Whether a process's `comm` and NUL-separated `cmdline` are clipman's: the stand-alone
/// `xfce4-clipman`, or the panel plugin running in the panel's wrapper.
#[must_use]
pub fn is_clipman(comm: &[u8], cmdline: &[u8]) -> bool {
    comm.strip_suffix(b"\n").unwrap_or(comm) == b"xfce4-clipman"
        || cmdline.split(|&b| b == 0).any(|arg| {
            arg == PLUGIN_ID.as_bytes()
                || arg.ends_with(b"/libclipman.so")
                || arg == b"libclipman.so"
        })
}

/// Whether a process of this user is clipman, looking in `proc` (normally `/proc`).
#[must_use]
pub fn clipman_running(proc: &Path) -> bool {
    let uid = rustix::process::getuid().as_raw();
    let Ok(entries) = fs::read_dir(proc) else { return false };
    entries.filter_map(Result::ok).any(|e| {
        let is_pid = e
            .file_name()
            .to_str()
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        is_pid
            && fs::metadata(e.path()).is_ok_and(|m| m.uid() == uid)
            && is_clipman(
                &fs::read(e.path().join("comm")).unwrap_or_default(),
                &fs::read(e.path().join("cmdline")).unwrap_or_default(),
            )
    })
}

/// Whether `xfconf-query -c xfce4-panel -p /plugins -lv`'s output lists the clipman plugin: lines
/// of a property and its value, the value of a plugin's own property being its id.
#[must_use]
pub fn panel_lists_clipman(output: &str) -> bool {
    output.lines().any(|line| {
        let mut fields = line.split_whitespace();
        let property = fields.next().unwrap_or_default();
        let value = fields.next();
        property.strip_prefix("/plugins/plugin-").is_some_and(|n| !n.contains('/'))
            && matches!(value, Some(PLUGIN_ID | "clipman"))
    })
}

/// Whether the Xfce panel lists the clipman plugin. Without `xfconf-query`, or if it does not
/// answer in time, the answer is no: the process check and the engine's still stand.
#[must_use]
pub fn clipman_in_panel() -> bool {
    let Ok(mut child) = Command::new("xfconf-query")
        .args(["-c", "xfce4-panel", "-p", "/plugins", "-lv"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    // Read on a thread of its own, so a large output cannot fill the pipe and stall it.
    let (tx, rx) = mpsc::channel();
    if let Some(mut stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            let mut out = String::new();
            let _ = stdout.read_to_string(&mut out);
            let _ = tx.send(out);
        });
    }
    let out = rx.recv_timeout(XFCONF_TIMEOUT);
    let _ = child.kill();
    let _ = child.wait();
    out.is_ok_and(|out| panel_lists_clipman(&out))
}

/// Programs that share a keyboard and mouse as input sharing does: their barriers and grabs would
/// fight ours (docs/input.md, "Coexistence"). Each name is at most 15 bytes, as `comm` keeps it.
pub const INPUT_RIVALS: [&str; 16] = [
    "synergy",
    "synergys",
    "synergyc",
    "synergy-core",
    "synergy-server",
    "synergy-client",
    "deskflow",
    "deskflow-server",
    "deskflow-client",
    "deskflow-core",
    "barrier",
    "barriers",
    "barrierc",
    "input-leap",
    "input-leaps",
    "input-leapc",
];

/// Whether a process's `comm` is one of [`INPUT_RIVALS`]; which.
#[must_use]
pub fn input_rival_named(comm: &[u8]) -> Option<&'static str> {
    let comm = comm.strip_suffix(b"\n").unwrap_or(comm);
    INPUT_RIVALS.iter().copied().find(|r| r.as_bytes() == comm)
}

/// The first process in `proc` (normally `/proc`) that is one of [`INPUT_RIVALS`], whoever runs
/// it.
#[must_use]
pub fn input_rival(proc: &Path) -> Option<&'static str> {
    let entries = fs::read_dir(proc).ok()?;
    entries.filter_map(Result::ok).find_map(|e| {
        let is_pid = e
            .file_name()
            .to_str()
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        if !is_pid {
            return None;
        }
        input_rival_named(&fs::read(e.path().join("comm")).unwrap_or_default())
    })
}
