//! Refusals made before the X server is reached, so no Xvfb is needed. In a binary of its own: the
//! stand-in clipman below would make every daemon started meanwhile refuse.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[track_caller]
fn ok<T, E: std::fmt::Debug>(r: Result<T, E>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("{e:?}"),
    }
}

fn home(name: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("refusals-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for d in ["state", "run", "config", "cache", "bin"] {
        ok(std::fs::create_dir_all(root.join(d)));
    }
    root
}

/// The exit status of a daemon that must refuse, within 10 s; `path` is put first on its `PATH`.
fn status(root: &Path, path: Option<&Path>) -> Option<i32> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_jackstayd"));
    command
        .args(["--display", ":0"])
        .arg("--state-dir")
        .arg(root.join("state"))
        .arg("--runtime-dir")
        .arg(root.join("run").join("jackstay"))
        .arg("--config")
        .arg(root.join("config").join("config.toml"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env_remove("NOTIFY_SOCKET")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(dir) = path {
        let rest = std::env::var_os("PATH").unwrap_or_default();
        let mut joined = dir.as_os_str().to_owned();
        joined.push(":");
        joined.push(rest);
        command.env("PATH", joined);
    }
    let mut child = ok(command.spawn());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = ok(child.try_wait()) {
            return status.code();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("jackstayd did not refuse");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn an_unreadable_configuration_is_refused() {
    let root = home("config");
    ok(std::fs::create_dir(root.join("config").join("config.toml")));
    assert_eq!(status(&root, None), Some(2));
}

#[test]
fn clipman_running_or_in_the_panel_is_refused() {
    let root = home("clipman");
    // The panel lists the plugin, though nothing runs it now.
    let bin = root.join("bin");
    let query = bin.join("xfconf-query");
    ok(std::fs::write(&query, "#!/bin/sh\necho '/plugins/plugin-4   xfce4-clipman-plugin'\n"));
    ok(std::fs::set_permissions(&query, std::os::unix::fs::PermissionsExt::from_mode(0o755)));
    assert_eq!(status(&root, Some(&bin)), Some(3));
    ok(std::fs::remove_file(&query));
    // It runs, though not from the panel.
    // A script's process is named after the script. (/bin/sleep, copied, refuses another name.)
    let clipman = root.join("xfce4-clipman");
    ok(std::fs::write(
        &clipman,
        "#!/bin/sh\ntrap 'kill $! 2>/dev/null; exit 0' TERM\nsleep 30 &\nwait\n",
    ));
    ok(std::fs::set_permissions(&clipman, std::os::unix::fs::PermissionsExt::from_mode(0o755)));
    let mut running = ok(Command::new(&clipman).spawn());
    std::thread::sleep(Duration::from_millis(200));
    let comm = std::fs::read_to_string(format!("/proc/{}/comm", running.id())).unwrap_or_default();
    assert_eq!(comm, "xfce4-clipman\n", "the stand-in runs, under clipman's name");
    let refused = status(&root, Some(&bin));
    // TERM, which it handles by stopping its sleep too.
    let _ = Command::new("kill").arg(running.id().to_string()).status();
    let _ = running.wait();
    assert_eq!(refused, Some(3));
}
