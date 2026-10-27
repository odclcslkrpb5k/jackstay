//! Xfce's settings daemon beside jackstayd: the real `xfsettingsd`, on Xvfb and a private session
//! bus. It runs a clipboard manager of its own (libxfce4ui's `XfceClipboardManager`) unless it
//! starts with `XFSETTINGSD_NO_CLIPBOARD` set (docs/daemon.md, "Coexistence").

mod support;

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use support::*;
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

/// A session bus of its own, whose activated services (xfconfd) get `home`.
struct Bus {
    child: Child,
    address: String,
}

impl Bus {
    fn start(home: &Home) -> Self {
        // dbus-daemon will not start for a uid with no passwd entry, which is how CI runs the
        // container: nss_wrapper gives it one, for this process alone (as crates/ui/tests/tray.rs).
        let ids = home.root.join("bus-ids");
        ok(std::fs::create_dir_all(&ids));
        let (uid, gid) = (rustix::process::getuid().as_raw(), rustix::process::getgid().as_raw());
        let passwd = format!("tester:x:{uid}:{gid}:tester:{}:/bin/sh\n", home.root.display());
        ok(std::fs::write(ids.join("passwd"), passwd));
        ok(std::fs::write(ids.join("group"), format!("tester:x:{gid}:\n")));
        let mut child = ok(Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .env("LD_PRELOAD", "libnss_wrapper.so")
            .env("NSS_WRAPPER_PASSWD", ids.join("passwd"))
            .env("NSS_WRAPPER_GROUP", ids.join("group"))
            .env("HOME", &home.root)
            .env("XDG_CONFIG_HOME", home.root.join("xdg-config"))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn());
        let mut address = String::new();
        let _ = BufReader::new(child.stdout.take().unwrap_or_else(|| panic!("no stdout")))
            .read_line(&mut address);
        let address = address.trim().to_owned();
        assert!(address.starts_with("unix:"), "{address:?}");
        Bus { child, address }
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The window owning `selection` on `x`, or 0.
fn owner_of(x: &Xvfb, selection: &str) -> u32 {
    let (conn, _) = ok(RustConnection::connect(Some(&x.display)));
    let atom = ok(ok(conn.intern_atom(false, selection.as_bytes())).reply()).atom;
    ok(ok(conn.get_selection_owner(atom)).reply()).owner
}

/// `xfsettingsd` as xfce4-session starts it, with or without its clipboard manager; returned once
/// it owns the XSETTINGS selection. Its clipboard manager is made just after, in the same
/// callback, and only if it can take `CLIPBOARD_MANAGER`.
fn xfsettingsd(x: &Xvfb, bus: &Bus, home: &Home, clipboard: bool) -> Child {
    let mut c = x.command("xfsettingsd");
    c.arg("--no-daemon")
        .env("DBUS_SESSION_BUS_ADDRESS", &bus.address)
        .env("HOME", &home.root)
        .env("XDG_CONFIG_HOME", home.root.join("xdg-config"))
        .env_remove("SESSION_MANAGER")
        .stdout(Stdio::null())
        .stderr(ok(std::fs::File::create(home.root.join("xfsettingsd.log"))));
    if clipboard {
        c.env_remove("XFSETTINGSD_NO_CLIPBOARD");
    } else {
        c.env("XFSETTINGSD_NO_CLIPBOARD", "1");
    }
    let mut child = ok(c.spawn());
    let deadline = Instant::now() + Duration::from_secs(20);
    while owner_of(x, "_XSETTINGS_S0") == 0 {
        let exited = ok(child.try_wait());
        if exited.is_some() || Instant::now() >= deadline {
            let log = std::fs::read_to_string(home.root.join("xfsettingsd.log"));
            panic!("xfsettingsd did not start (exited: {exited:?}); its log: {log:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child
}

#[test]
fn beside_xfsettingsds_clipboard_manager_it_refuses_and_says_how_to_turn_it_off() {
    let (x, home) = (Xvfb::start(), Home::new());
    let bus = Bus::start(&home);
    let settings = xfsettingsd(&x, &bus, &home, true);
    eventually("xfsettingsd to take CLIPBOARD_MANAGER", || owner_of(&x, "CLIPBOARD_MANAGER") != 0);
    assert_eq!(refused(Daemon::spawn(&x, &home)), Some(3));
    let log = ok(std::fs::read_to_string(home.root.join("daemon.log")));
    assert!(log.contains("owns CLIPBOARD_MANAGER: xfsettingsd,"), "{log}");
    assert!(log.contains("XFSETTINGSD_NO_CLIPBOARD=1 xfsettingsd --replace --daemon"), "{log}");
    kill(settings);
}

#[test]
fn beside_xfsettingsd_started_without_it_it_runs() {
    let (x, home) = (Xvfb::start(), Home::new());
    let bus = Bus::start(&home);
    let settings = xfsettingsd(&x, &bus, &home, false);
    assert_eq!(owner_of(&x, "CLIPBOARD_MANAGER"), 0);
    let daemon = Daemon::start(&x, &home);
    assert_ne!(owner_of(&x, "CLIPBOARD_MANAGER"), 0);
    let source = copy(&x, "copied beside xfsettingsd");
    eventually("the copy kept", || {
        previews(&home).first().map(String::as_str) == Some("copied beside xfsettingsd")
    });
    kill(source);
    assert_eq!(paste(&x).as_deref(), Some("copied beside xfsettingsd"));
    assert_eq!(daemon.stop(), Some(0));
    kill(settings);
}

#[test]
fn xfsettingsd_starting_after_it_leaves_the_manager_alone() {
    // At login xfsettingsd starts first; this is the other order, as a restart of it would be.
    let (x, home) = (Xvfb::start(), Home::new());
    let bus = Bus::start(&home);
    let daemon = Daemon::start(&x, &home);
    let ours = owner_of(&x, "CLIPBOARD_MANAGER");
    let settings = xfsettingsd(&x, &bus, &home, true);
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(owner_of(&x, "CLIPBOARD_MANAGER"), ours);
    assert_eq!(daemon.stop(), Some(0));
    kill(settings);
}
