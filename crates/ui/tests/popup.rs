//! The popup end to end: the real `jackstay-ui` on a private Xvfb, driven with xdotool as a user
//! would, against a stand-in daemon that records what it is asked (docs/ui.md, "The popup").
mod support;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use jackstay_proto::control::{Kinds, Request, Response, Summary, Thumb, Thumbnail};
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
    // Enter straight after the last letter, as a quick typist presses it.
    ui.x.xdotool(&["type", "--delay", "40", "PASS"]);
    ui.x.xdotool(&["key", "Return"]);
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
            // Built under a name of its own and renamed into place, so another test process
            // never runs it half-written.
            let building = out.with_extension(format!("{}.tmp", std::process::id()));
            let built = ok(Command::new("cc")
                .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-o"])
                .arg(&building)
                .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/helpers/grabber.c"))
                .args(flags.split_whitespace())
                .status());
            assert!(built.success(), "grabber did not build");
            ok(std::fs::rename(&building, &out));
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

const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];

/// A `width` by `height` thumbnail of one colour, of an image `image` times larger.
fn solid(width: u16, height: u16, colour: [u8; 4], image: u32) -> Thumb {
    let rgba = colour.repeat(usize::from(width) * usize::from(height));
    Thumb::Ready(ok(Thumbnail::new(
        width,
        height,
        u32::from(width) * image,
        u32::from(height) * image,
        rgba,
    )))
}

fn image_entry(id: u64) -> Summary {
    Summary {
        kinds: Kinds(Kinds::IMAGE),
        preview: String::new(),
        size: 5000,
        ..entry(id, "", false)
    }
}

/// How many pixels on the screen are exactly `colour`.
fn on_screen(x: &Xvfb, colour: [u8; 4]) -> usize {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat};
    let (conn, screen) = ok(x11rb::rust_connection::RustConnection::connect(Some(&x.display)));
    let root = conn.setup().roots[screen].root;
    let image = ok(ok(conn.get_image(ImageFormat::Z_PIXMAP, root, 0, 0, 1024, 768, !0)).reply());
    // Depth 24 at 32 bits a pixel, least significant byte first: blue, green, red, unused.
    let want = [colour[2], colour[1], colour[0]];
    image.data.as_chunks::<4>().0.iter().filter(|p| p[..3] == want).count()
}

#[test]
fn image_rows_show_their_thumbnails_as_they_arrive() {
    let ui = start("thumbs");
    let entries =
        vec![entry(14, "some text", true), image_entry(13), image_entry(12), image_entry(11)];
    let daemon = Daemon::start(&ui.runtime, entries);
    daemon.thumbnail(13, vec![Some(solid(40, 20, RED, 10))]);
    // Still being made the first three times it is asked.
    let pending = Some(Thumb::Pending);
    let green = Some(solid(20, 20, GREEN, 1));
    daemon.thumbnail(12, vec![pending.clone(), pending.clone(), pending, green]);
    daemon.thumbnail(11, vec![Some(Thumb::Absent)]);
    ui.popup();
    ui.keys(&["Down", "Down"]);
    eventually("the first thumbnail, unscaled", || on_screen(&ui.x, RED) == 40 * 20);
    eventually("the second, once made", || on_screen(&ui.x, GREEN) == 20 * 20);
    // Filled in where it was: the selection did not move.
    ui.keys(&["Return"]);
    ui.closed();
    assert_eq!(daemon.changes(), [Request::Select { id: 12 }]);
    let asked = daemon.thumbnails_asked();
    let count = |id| asked.iter().filter(|&&a| a == id).count();
    assert_eq!((count(14), count(13), count(11)), (0, 1, 1), "{asked:?}");
    assert_eq!(count(12), 4, "{asked:?}");
    // Known ones are not asked again.
    ui.popup();
    eventually("shown again", || {
        on_screen(&ui.x, GREEN) == 20 * 20 && on_screen(&ui.x, RED) == 800
    });
    assert_eq!(daemon.thumbnails_asked().len(), asked.len());
    ui.keys(&["Escape"]);
    ui.closed();
}

#[test]
fn a_thumbnail_that_failed_is_asked_again_next_time() {
    let ui = start("th-fail");
    let daemon = Daemon::start(&ui.runtime, vec![image_entry(13)]);
    // The daemon restarting, say: an error the first time.
    daemon.thumbnail(13, vec![None, Some(solid(40, 20, RED, 2))]);
    ui.popup();
    eventually("asked once", || daemon.thumbnails_asked().len() == 1);
    ui.keys(&["Escape"]);
    ui.closed();
    ui.popup();
    eventually("shown the second time", || on_screen(&ui.x, RED) == 40 * 20);
    assert_eq!(daemon.thumbnails_asked(), [13, 13]);
}

#[test]
fn listing_again_does_not_ask_more_often() {
    let ui = start("th-list");
    let daemon = Daemon::start(&ui.runtime, vec![image_entry(13), entry(12, "text", false)]);
    daemon.thumbnail(13, vec![Some(Thumb::Pending)]);
    ui.popup();
    // Each pin lists again, while 13 is still being made.
    for _ in 0..5 {
        ui.keys(&["ctrl+p"]);
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_secs(3));
    ui.keys(&["Escape"]);
    ui.closed();
    // Asked at 0, then after 0.2, 0.4, 0.6, 0.8, 1 and 1 s: 7 in 3.5 s, however often listed.
    let asked = daemon.thumbnails_asked().len();
    assert!((5..=8).contains(&asked), "asked {asked} times");
    // Closed: no longer asked.
    std::thread::sleep(Duration::from_millis(1500));
    assert!(daemon.thumbnails_asked().len() <= asked + 1);
}
