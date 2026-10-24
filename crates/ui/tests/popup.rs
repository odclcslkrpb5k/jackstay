//! The popup end to end: the real `jackstay-ui` on a private Xvfb, driven with xdotool as a user
//! would, against a stand-in daemon that records what it is asked (docs/ui.md, "The popup").
mod support;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use jackstay_proto::control::{Request, Response};
use jackstayd::ui_socket;
use support::*;

#[test]
fn enter_pastes_the_selected_entry_and_escape_only_closes() {
    let ui = start("enter");
    let daemon = Daemon::start(&ui.runtime, history());
    ui.popup();
    ui.keys(&["Escape"]);
    ui.closed();
    assert_eq!(daemon.changes(), []);
    ui.popup();
    ui.keys(&["Return"]);
    ui.closed();
    ui.popup();
    ui.keys(&["Down", "Down", "Return"]);
    ui.closed();
    assert_eq!(daemon.changes(), [Request::Select { id: 14 }, Request::Select { id: 12 }]);
}

#[test]
fn typing_searches_and_enter_pastes_the_first_match() {
    let ui = start("search");
    let daemon = Daemon::start(&ui.runtime, history());
    ui.popup();
    ui.x.xdotool(&["type", "--delay", "40", "PASS"]);
    std::thread::sleep(Duration::from_millis(200));
    ui.keys(&["Return"]);
    ui.closed();
    // Searched again from scratch next time: Enter is the newest.
    ui.popup();
    ui.keys(&["Return"]);
    ui.closed();
    assert_eq!(daemon.changes(), [Request::Select { id: 13 }, Request::Select { id: 14 }]);
}

#[test]
fn delete_and_pin_keep_the_popup_open_and_list_again() {
    let ui = start("edit");
    let daemon = Daemon::start(&ui.runtime, history());
    ui.popup();
    let listed = daemon.lists();
    ui.keys(&["Down", "ctrl+d"]);
    eventually("listed again", || daemon.lists() > listed);
    assert!(ui.visible());
    // The deleted entry is gone: the second row is now the third entry.
    ui.keys(&["Down", "ctrl+p", "Escape"]);
    ui.closed();
    assert_eq!(
        daemon.changes(),
        [Request::Delete { id: 13 }, Request::Pin { id: 12, pinned: true }]
    );
}

#[test]
fn clearing_takes_one_press_and_clearing_history_two() {
    let ui = start("clear");
    let daemon = Daemon::start(&ui.runtime, history());
    ui.popup();
    ui.keys(&["alt+c"]);
    ui.closed();
    ui.popup();
    ui.keys(&["alt+h", "Escape"]);
    ui.closed();
    assert_eq!(daemon.changes(), [Request::Clear], "one press of clear history does nothing");
    ui.popup();
    ui.keys(&["alt+h"]);
    assert!(ui.visible(), "asks first");
    ui.keys(&["alt+h"]);
    ui.closed();
    assert_eq!(daemon.changes(), [Request::Clear, Request::ClearHistory]);
}

#[test]
fn a_click_outside_closes_it() {
    let ui = start("click");
    let _daemon = Daemon::start(&ui.runtime, history());
    ui.x.xdotool(&["mousemove", "400", "300"]);
    ui.popup();
    ui.x.xdotool(&["mousemove", "5", "5", "click", "1"]);
    ui.closed();
}

#[test]
fn without_a_daemon_it_still_opens_to_say_so() {
    let ui = start("nodaemon");
    ui.popup();
    ui.keys(&["Return", "Escape"]);
    ui.closed();
}

#[test]
fn a_second_ui_is_refused() {
    let ui = start("second");
    let status = ok(ui_command(&ui.x, &ui.runtime).status());
    assert_eq!(status.code(), Some(3));
    // The first still answers.
    let _daemon = Daemon::start(&ui.runtime, history());
    ui.popup();
}

#[test]
fn page_and_arrow_keys_move_within_the_list() {
    let ui = start("keys");
    let daemon = Daemon::start(&ui.runtime, history());
    for keys in [
        &["Page_Down", "Return"][..],
        &["Page_Down", "Up", "Return"],
        &["Down", "Down", "Page_Up", "Return"],
        &["Up", "Return"],
    ] {
        ui.popup();
        ui.keys(keys);
        ui.closed();
    }
    assert_eq!(
        daemon.changes(),
        [
            Request::Select { id: 11 },
            Request::Select { id: 12 },
            Request::Select { id: 14 },
            Request::Select { id: 14 },
        ]
    );
}

/// The letters that pin, delete and clear with a modifier are only search text without one.
#[test]
fn plain_letters_only_search() {
    let ui = start("letters");
    let daemon = Daemon::start(&ui.runtime, history());
    ui.popup();
    ui.x.xdotool(&["type", "--delay", "40", "dpchDPCH"]);
    std::thread::sleep(Duration::from_millis(200));
    ui.keys(&["Escape"]);
    ui.closed();
    assert_eq!(daemon.changes(), []);
}

/// A paste the daemon could not make (another application holds the clipboard) opens the popup
/// again to say so, rather than leave the clipboard silently unchanged.
#[test]
fn a_refused_paste_reopens_the_popup() {
    let ui = start("refused");
    let daemon = Daemon::answering(
        &ui.runtime,
        history(),
        Response::Error(jackstay_proto::control::Error::ClaimFailed),
    );
    ui.popup();
    ui.keys(&["Return"]);
    eventually("the paste asked", || !daemon.changes().is_empty());
    eventually("the popup to open again", || ui.visible());
    ui.keys(&["Escape"]);
    ui.closed();
}

/// Opened at the pointer, but never past its monitor's edge.
#[test]
fn it_opens_at_the_pointer_within_the_screen() {
    let ui = start("place");
    let _daemon = Daemon::start(&ui.runtime, history());
    for (x, y) in [(100, 120), (1020, 765)] {
        ui.x.xdotool(&["mousemove", &x.to_string(), &y.to_string()]);
        ui.popup();
        let out = ok(ui
            .x
            .command("xdotool")
            .args([
                "search",
                "--onlyvisible",
                "--name",
                "Jackstay history",
                "getwindowgeometry",
                "--shell",
            ])
            .output());
        let shell = String::from_utf8_lossy(&out.stdout).into_owned();
        let field = |name: &str| -> i32 {
            shell
                .lines()
                .find_map(|l| l.strip_prefix(&format!("{name}=")))
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| panic!("no {name} in {shell}"))
        };
        let (left, top, width, height) = (field("X"), field("Y"), field("WIDTH"), field("HEIGHT"));
        assert!(left >= 0 && top >= 0, "{shell}");
        assert!(left + width <= 1024 && top + height <= 768, "{shell}");
        if x == 100 {
            assert_eq!((left, top), (100, 120), "at the pointer");
        }
        ui.keys(&["Escape"]);
        ui.closed();
    }
}

/// Built once per run from tests/helpers.
fn grabber() -> PathBuf {
    static BUILT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("grabber");
            let flags =
                ok(Command::new("pkg-config").args(["--cflags", "--libs", "gtk+-3.0"]).output());
            let flags = String::from_utf8_lossy(&flags.stdout).into_owned();
            let built = ok(Command::new("cc")
                .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-o"])
                .arg(&out)
                .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/helpers/grabber.c"))
                .args(flags.split_whitespace())
                .status());
            assert!(built.success(), "grabber did not build");
            out
        })
        .clone()
}

/// Runs the grabber helper (`mode`, `ms`) and waits for it to report it has done so.
fn hold(x: &Xvfb, mode: &str, ms: u32) -> Child {
    let mut child = ok(x
        .command(grabber().to_str().unwrap_or_default())
        .args([mode, &ms.to_string()])
        .env("GDK_BACKEND", "x11")
        .env("NO_AT_BRIDGE", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn());
    let mut line = String::new();
    let _ = BufReader::new(child.stdout.take().unwrap_or_else(|| panic!("no stdout")))
        .read_line(&mut line);
    assert!(line == "grabbed\n" || line == "focused\n", "{mode}: {line:?}");
    child
}

/// A hotkey's own grab can still be held as the popup opens: it takes the keyboard once that
/// lets go. Meanwhile another window has the input focus, as the application a window manager
/// keeps focused would, so only the popup's grab brings it the keys typed next.
#[test]
fn it_takes_the_keyboard_once_a_hotkeys_grab_lets_go() {
    let ui = start("grab");
    let daemon = Daemon::start(&ui.runtime, history());
    let mut holder = hold(&ui.x, "grab", 400);
    ui.x.xdotool(&["mousemove", "500", "400"]);
    ui.popup();
    let mut focused = hold(&ui.x, "focus", 5_000);
    let _ = holder.wait();
    std::thread::sleep(Duration::from_millis(150));
    ui.x.xdotool(&["mousemove", "2", "2"]);
    ui.keys(&["Down", "Return"]);
    ui.closed();
    let _ = focused.kill();
    let _ = focused.wait();
    assert_eq!(daemon.changes(), [Request::Select { id: 13 }]);
}

/// A delete the daemon refused leaves the popup as it was, search and all, saying why.
#[test]
fn a_refused_delete_keeps_the_search() {
    let ui = start("refused-delete");
    let daemon = Daemon::refusing_deletes(&ui.runtime, history());
    ui.popup();
    ui.x.xdotool(&["type", "--delay", "40", "third"]);
    std::thread::sleep(Duration::from_millis(200));
    ui.keys(&["ctrl+d"]);
    eventually("the delete asked", || !daemon.changes().is_empty());
    std::thread::sleep(Duration::from_millis(200));
    ui.keys(&["Return"]);
    ui.closed();
    assert_eq!(daemon.changes(), [Request::Delete { id: 12 }, Request::Select { id: 12 }]);
}

#[test]
fn the_runtime_directory_defaults_to_xdgs_and_bad_arguments_are_refused() {
    let x = Xvfb::start();
    let xdg = runtime("xdg");
    let mut child = ok(x
        .command(env!("CARGO_BIN_EXE_jackstay-ui"))
        .env("XDG_RUNTIME_DIR", &xdg)
        .env("GDK_BACKEND", "x11")
        .env("NO_AT_BRIDGE", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn());
    let path = ui_socket::path(&xdg.join("jackstay"));
    eventually("the socket under XDG_RUNTIME_DIR", || path.exists());
    let _ = child.kill();
    let _ = child.wait();
    // Two arguments, but not `--runtime-dir`, naming a directory it could have used.
    let mut bad = ok(x
        .command(env!("CARGO_BIN_EXE_jackstay-ui"))
        .arg("--state-dir")
        .arg(runtime("bad"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn());
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = ok(bad.try_wait()) {
            break status;
        }
        if Instant::now() > deadline {
            let _ = bad.kill();
            panic!("started with bad arguments");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(2));
}
