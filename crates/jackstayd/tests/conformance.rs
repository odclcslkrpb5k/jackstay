//! The conformance matrix's core tier (docs/conformance.md): real clipboard clients, each toolkit's
//! own clipboard code, copying and pasting through the real daemon on Xvfb.
//!
//! For each source and payload: what every sink pastes while the source lives is what the source
//! serves; the daemon keeps exactly the canonical target of each class offered (docs/plan.md
//! §5.2); and once the source has quit its own way, every sink pastes the same bytes from the
//! daemon, which offers nothing it does not hold.

#[path = "conformance/matrix.rs"]
mod matrix;
mod support;

use std::io::Write;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use matrix::{
    Files, GTK4_LARGE, GTK4_TEXT, Kit, LARGE, Owner, Source, XSEL_MAX, client_name, newest, paste,
    png, rig, row, targets, text, toolkit, xterm,
};
use support::{Daemon, Home, Xvfb, eventually, ok};

#[test]
fn xclip_and_xsel_as_sources() {
    let r = rig();
    let small = r.files.write("small.txt", &text(64));
    let large = r.files.write("large.txt", &text(LARGE));
    let html = r.files.write("small.html", b"<b>bold</b> &amp; <i>more</i>");
    let image = r.files.write("small.png", &png(16, 16));
    let big_image = r.files.write("large.png", &png(2600, 2600));
    let uris = r.files.write("uris", b"file:///tmp/a%20b.txt\r\nfile:///tmp/c.txt\r\n");
    for (target, file, what) in [
        ("UTF8_STRING", &small, "xclip small text"),
        ("UTF8_STRING", &large, "xclip large text"),
        ("text/html", &html, "xclip html"),
        ("image/png", &image, "xclip small png"),
        ("image/png", &big_image, "xclip large png"),
        ("text/uri-list", &uris, "xclip uri list"),
    ] {
        let kept = row(&r.x, &r.home, Kit::Xclip, &[(target, file)], what);
        assert_eq!(kept.get(target), Some(&ok(std::fs::read(file))), "{what}: as given");
    }
    row(&r.x, &r.home, Kit::Xsel, &[("text", &small)], "xsel small text");
    let medium = r.files.write("medium.txt", &text(XSEL_MAX));
    row(&r.x, &r.home, Kit::Xsel, &[("text", &medium)], "xsel 1 MiB text");
}

fn toolkit_rows(kit: Kit) {
    let r = rig();
    let name = format!("{kit:?}");
    let small = r.files.write("small.txt", &text(64));
    // GTK 4 cannot serve its own text past 256 KiB on X11, nor raw bytes from 16 MiB
    // (docs/conformance.md, "What the toolkits cannot do").
    let large = r.files.write("large.txt", &text(if kit == Kit::Gtk4 { GTK4_TEXT } else { LARGE }));
    let large_raw =
        r.files.write("large-raw.txt", &text(if kit == Kit::Gtk4 { GTK4_LARGE } else { LARGE }));
    let html = r.files.write("small.html", b"<b>bold</b> &amp; <i>more</i>");
    let image = r.files.write("small.png", &png(16, 16));
    let medium_image = r.files.write("medium.png", &png(600, 600));
    let big_raw = r
        .files
        .write("large.png", &if kit == Kit::Gtk4 { png(2000, 2000) } else { png(2600, 2600) });
    let uris = r.files.write("uris", b"file:///tmp/a%20b.txt\nfile:///tmp/c.txt\n");
    let text_target = if kit == Kit::Gtk3 { "UTF8_STRING" } else { "text/plain;charset=utf-8" };
    let rows: Vec<(Vec<(&str, &Path)>, &str)> = vec![
        (vec![("text", &small)], "toolkit small text"),
        (vec![("text", &large)], "toolkit large text"),
        (vec![("text", &small), ("text/html", &html)], "toolkit text and html"),
        (vec![("image", &image)], "toolkit small image"),
        (vec![("image", &medium_image)], "toolkit medium image"),
        (vec![("uris", &uris)], "toolkit uri list"),
        (vec![(text_target, &small)], "raw text"),
        (vec![(text_target, &large_raw)], "raw large text"),
        (vec![("image/png", &big_raw)], "raw large png"),
    ];
    for (offer, what) in &rows {
        row(&r.x, &r.home, kit, offer, &format!("{name} {what}"));
    }
}

#[test]
fn gtk3_as_source() {
    toolkit_rows(Kit::Gtk3);
}

#[test]
fn gtk4_as_source() {
    toolkit_rows(Kit::Gtk4);
}

#[test]
fn qt6_as_source() {
    toolkit_rows(Kit::Qt6);
}

/// A toolkit that copies and quits at once hands its targets to the clipboard manager on the way
/// out (`SAVE_TARGETS`), so nothing depends on the daemon having seen the copy in time.
#[test]
fn a_copy_made_just_before_quitting_is_kept() {
    let r = rig();
    let small = r.files.write("small.txt", &text(200));
    for kit in [Kit::Gtk3, Kit::Qt6] {
        let mut child = ok(toolkit(&r.x, &r.home, client_name(kit))
            .args(["copy", "text"])
            .arg(&small)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn());
        let began = Instant::now();
        let status = ok(child.wait());
        assert!(status.success(), "{kit:?}");
        assert!(began.elapsed() < Duration::from_secs(5), "{kit:?} waited on the manager");
        eventually(&format!("{kit:?}'s copy kept"), || {
            paste(Kit::Xclip, &r.x, &r.home, "UTF8_STRING").as_deref()
                == Some(&ok(std::fs::read(&small))[..])
        });
    }
}

/// A target over `capture.per_target` is not kept: it is served while its owner lives, and gone
/// once the owner is, without taking the rest of the entry with it.
#[test]
fn a_target_over_the_limit_is_not_kept_but_the_rest_is() {
    let x = Xvfb::start();
    let home = Home::new();
    ok(std::fs::write(home.config(), "[capture]\nper_target = 1048576\n"));
    let d = Daemon::start(&x, &home);
    let files = Files::new(&home);
    let at = files.write("at.txt", &text(1 << 20));
    let over = files.write("over.png", &vec![7u8; (1 << 20) + 1]);
    let source = Source::start(Kit::Gtk3, &x, &home, &[("UTF8_STRING", &at), ("image/png", &over)]);
    eventually("the entry", || newest(&home).is_some_and(|s| s.size == 1 << 20));
    let _ = source.quit();
    eventually("the daemon serving the text", || {
        paste(Kit::Xclip, &x, &home, "UTF8_STRING").map(|b| b.len()) == Some(1 << 20)
    });
    assert_eq!(paste(Kit::Xclip, &x, &home, "image/png"), None);
    assert_eq!(targets(&x).map(|t| t.contains(&"image/png".to_owned())), Some(false));
    drop(d);
    let _ = std::io::stdout().flush();
}

#[test]
fn xterm_as_source() {
    let r = rig();
    let line = r.files.write("line.txt", "héllo wörld ✓ from xterm\nnot selected\n".as_bytes());
    let kept = row(&r.x, &r.home, Kit::Xterm, &[("text", &line)], "xterm line");
    assert_eq!(
        kept.get("UTF8_STRING").map(|b| String::from_utf8_lossy(b).into_owned()).as_deref(),
        Some("héllo wörld ✓ from xterm\n")
    );
}

/// xterm pastes what the daemon kept once its owner is gone, through its own UTF-8 path, into
/// `cat` on its tty.
#[test]
fn xterm_pastes_what_the_daemon_kept() {
    let r = rig();
    let want = "pästed ☃ by xterm\nsecond line\n";
    let file = r.files.write("paste.txt", want.as_bytes());
    let source = Source::start(Kit::Xclip, &r.x, &r.home, &[("UTF8_STRING", &file)]);
    eventually("the copy kept", || newest(&r.home).is_some_and(|s| s.size == want.len() as u64));
    let _ = source.quit();
    eventually("the daemon serving it", || {
        paste(Kit::Xclip, &r.x, &r.home, "UTF8_STRING").as_deref() == Some(want.as_bytes())
    });
    let out = r.files.dir.join("xterm-out.txt");
    let mut term = ok(xterm(&r.x)
        .args(["-xrm", "XTerm*VT100.translations: #override <Key>F5: insert-selection(CLIPBOARD)"])
        .args(["-e", "sh", "-c"])
        .arg(format!("stty -echo; cat > '{}'", out.display()))
        .spawn());
    let found = ok(r
        .x
        .command("timeout")
        .args(["10", "xdotool", "search", "--sync", "--pid", &term.id().to_string()])
        .stdout(Stdio::null())
        .status());
    assert!(found.success(), "no xterm window");
    std::thread::sleep(Duration::from_millis(300));
    // The keyboard follows the pointer without a window manager.
    let pressed = ok(r.x.command("xdotool").args(["mousemove", "30", "30", "key", "F5"]).status());
    assert!(pressed.success());
    eventually("xterm's paste", || std::fs::read(&out).is_ok_and(|b| b == want.as_bytes()));
    let _ = term.kill();
    let _ = term.wait();
}
