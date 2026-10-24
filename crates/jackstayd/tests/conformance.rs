//! The conformance matrix's core tier (docs/conformance.md): real clipboard clients, each toolkit's
//! own clipboard code, copying and pasting through the real daemon on Xvfb.
//!
//! For each source and payload: what every sink pastes while the source lives is what the source
//! serves; the daemon keeps exactly the canonical target of each class offered (docs/plan.md
//! §5.2); and once the source has quit its own way, every sink pastes the same bytes from the
//! daemon, which offers nothing it does not hold.

mod support;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use jackstay_proto::control::{Request, Response, Summary};
use support::{Daemon, Home, Xvfb, ask, eventually, kill, ok};

// ---- The clients, built once per run from tests/conformance/clients.

fn clients() -> &'static Path {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/conformance/clients");
        let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("conformance-clients");
        ok(std::fs::create_dir_all(&out));
        let build = |compiler: &str, std: &str, file: &str, packages: &[&str], name: &str| {
            let flags = ok(Command::new("pkg-config")
                .arg("--cflags")
                .arg("--libs")
                .args(packages)
                .output());
            assert!(flags.status.success(), "pkg-config {packages:?}");
            let flags = String::from_utf8_lossy(&flags.stdout).into_owned();
            let status = ok(Command::new(compiler)
                .args([std, "-fPIC", "-Wall", "-Wextra", "-Werror", "-O1", "-o"])
                .arg(out.join(name))
                .arg(src.join(file))
                .args(flags.split_whitespace())
                .status());
            assert!(status.success(), "{name} did not build");
        };
        build("cc", "-std=c11", "gtk3.c", &["gtk+-3.0"], "clip-gtk3");
        build("cc", "-std=c11", "gtk4.c", &["gtk4", "gio-unix-2.0"], "clip-gtk4");
        build("c++", "-std=c++17", "qt6.cpp", &["Qt6Gui"], "clip-qt6");
        out
    })
}

/// A toolkit client, quiet and on X11 whatever the environment says.
fn toolkit(x: &Xvfb, home: &Home, name: &str) -> Command {
    let runtime = home.root.join("xdg");
    let _ = std::fs::create_dir(&runtime);
    let _ = std::fs::set_permissions(&runtime, std::os::unix::fs::PermissionsExt::from_mode(0o700));
    let mut c = x.command(clients().join(name).to_str().unwrap_or_default());
    c.env("GDK_BACKEND", "x11")
        .env("GSK_RENDERER", "cairo")
        .env("GTK_A11Y", "none")
        .env("NO_AT_BRIDGE", "1")
        .env("QT_QPA_PLATFORM", "xcb")
        .env("LANG", "C.UTF-8")
        .env("XDG_RUNTIME_DIR", &runtime)
        .stderr(Stdio::null());
    c
}

// ---- Sources.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kit {
    Xclip,
    Xsel,
    Gtk3,
    Gtk4,
    Qt6,
    /// As a source only: it shows a file, a triple click selects its first line onto CLIPBOARD.
    /// As a sink it pastes through its tty, which [`xterm_pastes_what_the_daemon_kept`] drives.
    Xterm,
}

/// What a source offers: each target (or a toolkit's `text`, `image` or `uris`) and a file.
type Offer<'a> = &'a [(&'a str, &'a Path)];

struct Source {
    child: Child,
    stdin: Option<ChildStdin>,
}

impl Source {
    fn start(kit: Kit, x: &Xvfb, home: &Home, offer: Offer<'_>) -> Self {
        let mut command = match kit {
            Kit::Xclip => {
                assert_eq!(offer.len(), 1, "xclip offers one target");
                let mut c = x.command("xclip");
                c.args(["-quiet", "-selection", "clipboard", "-t", offer[0].0, "-i"])
                    .arg(offer[0].1)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                c
            }
            Kit::Xsel => {
                let mut c = x.command("xsel");
                c.args(["--clipboard", "--input", "--nodetach"])
                    .stdin(ok(std::fs::File::open(offer[0].1)))
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                c
            }
            Kit::Xterm => {
                let mut c = xterm(x);
                c.args(["-xrm", "XTerm*selectToClipboard: true", "-e", "sh", "-c"])
                    .arg(format!("cat '{}'; exec sleep 600", offer[0].1.display()));
                c
            }
            Kit::Gtk3 | Kit::Gtk4 | Kit::Qt6 => {
                let mut c = toolkit(x, home, client_name(kit));
                c.arg("copy");
                for (target, file) in offer {
                    c.arg(target).arg(file);
                }
                c.stdin(Stdio::piped()).stdout(Stdio::piped());
                c
            }
        };
        let mut child = ok(command.spawn());
        if kit == Kit::Xterm {
            select_first_line(x, child.id());
        }
        let stdin = child.stdin.take();
        if let Some(out) = child.stdout.take() {
            let mut line = String::new();
            let _ = BufReader::new(out).read_line(&mut line);
            assert_eq!(line, "owned\n", "{kit:?} did not take the clipboard");
        }
        eventually("the source to own the clipboard", || owned_by_other(x, home));
        Source { child, stdin }
    }

    /// Quits as the application would (a toolkit through its own exit, with its handoff to the
    /// manager), or is killed (xclip, xsel); returns how long the exit took.
    fn quit(mut self) -> Duration {
        let began = Instant::now();
        if self.stdin.take().is_some() {
            let deadline = Instant::now() + Duration::from_secs(15);
            while ok(self.child.try_wait()).is_none() {
                assert!(Instant::now() < deadline, "the source did not quit");
                std::thread::sleep(Duration::from_millis(10));
            }
        } else {
            kill(self.child);
        }
        began.elapsed()
    }
}

fn client_name(kit: Kit) -> &'static str {
    match kit {
        Kit::Gtk3 => "clip-gtk3",
        Kit::Gtk4 => "clip-gtk4",
        Kit::Qt6 => "clip-qt6",
        Kit::Xclip | Kit::Xsel | Kit::Xterm => "",
    }
}

/// An xterm in UTF-8 at the top left, where the pointer can find its first line.
fn xterm(x: &Xvfb) -> Command {
    let mut c = x.command("xterm");
    c.env("LANG", "C.UTF-8")
        .args(["-u8", "-geometry", "80x10+0+0"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    c
}

/// Waits for xterm `pid`'s window, then triple-clicks its first line.
fn select_first_line(x: &Xvfb, pid: u32) {
    let found = ok(x
        .command("timeout")
        .args(["10", "xdotool", "search", "--sync", "--pid", &pid.to_string()])
        .stdout(Stdio::null())
        .status());
    assert!(found.success(), "no xterm window");
    // Its text is drawn once mapped; a click before that selects nothing.
    std::thread::sleep(Duration::from_millis(300));
    let clicked = ok(x
        .command("xdotool")
        .args(["mousemove", "30", "8", "click", "--repeat", "3", "--delay", "80", "1"])
        .status());
    assert!(clicked.success());
}

/// Whether CLIPBOARD is owned by someone other than the daemon, and answers TARGETS.
fn owned_by_other(x: &Xvfb, home: &Home) -> bool {
    let daemon = match ask(home, Request::Status) {
        Response::Status(s) => s.owns_clipboard,
        other => panic!("not a status: {other:?}"),
    };
    !daemon && targets(x).is_some_and(|t| !t.is_empty())
}

// ---- Sinks.

const SINKS: [Kit; 5] = [Kit::Xclip, Kit::Xsel, Kit::Gtk3, Kit::Gtk4, Kit::Qt6];

/// What `kit` pastes for the X target `target`, if it gets anything.
fn paste(kit: Kit, x: &Xvfb, home: &Home, target: &str) -> Option<Vec<u8>> {
    let mut command = match kit {
        Kit::Xclip => {
            let mut c = x.command("timeout");
            c.args(["10", "xclip", "-selection", "clipboard", "-t", target, "-o"]);
            c
        }
        Kit::Xterm => return None,
        Kit::Xsel => {
            if target != "UTF8_STRING" {
                return None;
            }
            let mut c = x.command("timeout");
            c.args(["10", "xsel", "--clipboard", "--output"]);
            c
        }
        Kit::Gtk3 => {
            let mut c = toolkit(x, home, "clip-gtk3");
            c.args(["paste", target]);
            c
        }
        // GTK 4 and Qt name text by MIME type; other targets are MIME types already.
        Kit::Gtk4 => {
            let mut c = toolkit(x, home, "clip-gtk4");
            c.args([
                "paste",
                if target == "UTF8_STRING" { "text/plain;charset=utf-8" } else { target },
            ]);
            c
        }
        Kit::Qt6 => {
            let mut c = toolkit(x, home, "clip-qt6");
            c.args(["paste", if target == "UTF8_STRING" { "text/plain" } else { target }]);
            c
        }
    };
    let out = ok(command.stdin(Stdio::null()).stderr(Stdio::null()).output());
    out.status.success().then_some(out.stdout)
}

/// Pairs of other programs that fail each other, whatever the daemon does (docs/conformance.md,
/// "What the toolkits cannot do"); not asked while the source lives, since a reader that gives
/// up mid-transfer leaves the owner stuck for every reader after it.
fn broken_pair(source: Kit, sink: Kit, target: &str, offered: &[String], len: usize) -> bool {
    let gtk4 = sink == Kit::Gtk4;
    // GTK 4 misreads xclip's INCR transfers: 1 MiB arrives as 1 byte.
    let xclip_incr = gtk4 && source == Kit::Xclip && len >= 1 << 20;
    // GTK 4 asks for `text/plain;charset=utf-8` as `UTF8_STRING`, and gets nothing from an owner
    // that offers only the former.
    let no_utf8 =
        gtk4 && target == "text/plain;charset=utf-8" && !offered.iter().any(|t| t == "UTF8_STRING");
    xclip_incr || no_utf8
}

/// Sinks that can ask for `target` at all, `len` bytes of it.
fn sinks_for(target: &str, len: usize) -> impl Iterator<Item = Kit> + '_ {
    SINKS
        .into_iter()
        .filter(move |k| *k != Kit::Xsel || (target == "UTF8_STRING" && len <= XSEL_MAX))
        // Qt folds `text/x-moz-url` into `text/uri-list` and never shows it as a format.
        .filter(move |k| *k != Kit::Qt6 || target != "text/x-moz-url")
}

fn targets(x: &Xvfb) -> Option<Vec<String>> {
    let out = ok(x
        .command("timeout")
        .args(["10", "xclip", "-selection", "clipboard", "-t", "TARGETS", "-o"])
        .stderr(Stdio::null())
        .output());
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).lines().map(str::to_owned).collect())
}

/// Never converted, so never kept (docs/selection.md, "Targets the engine never converts").
const NEVER: [&str; 12] = [
    "TARGETS",
    "MULTIPLE",
    "TIMESTAMP",
    "SAVE_TARGETS",
    "DELETE",
    "INSERT_PROPERTY",
    "INSERT_SELECTION",
    "LENGTH",
    "DRAWABLE",
    "BITMAP",
    "PIXMAP",
    "COLORMAP",
];
const TEXT: [&str; 3] = ["UTF8_STRING", "text/plain;charset=utf-8", "STRING"];
const ALWAYS: [&str; 4] = [
    "x-special/gnome-copied-files",
    "chromium/x-web-custom-data",
    "image/svg+xml",
    "image/x-inkscape-svg",
];

/// The targets the daemon keeps of those offered, restated from docs/selection.md ("Capture",
/// step 4) rather than taken from the engine.
fn canonical(offered: &[String]) -> Vec<String> {
    let usable: Vec<&str> =
        offered.iter().map(String::as_str).filter(|t| !NEVER.contains(t)).collect();
    let text = TEXT.into_iter().find(|t| usable.contains(t));
    let mut kept: Vec<&str> = text.into_iter().collect();
    for &t in &usable {
        let alias = text.is_some() && matches!(t, "text/plain" | "text/plain;charset=utf-8");
        if (t.starts_with("text/") && Some(t) != text && !alias) || ALWAYS.contains(&t) {
            kept.push(t);
        }
    }
    let raster = usable.iter().copied().find(|t| *t == "image/png").or_else(|| {
        usable.iter().copied().find(|t| t.starts_with("image/") && !ALWAYS.contains(t))
    });
    kept.extend(raster);
    let mut seen = std::collections::HashSet::new();
    kept.into_iter().filter(|t| seen.insert(*t)).map(str::to_owned).collect()
}

/// What the daemon must answer for a text alias of the text it keeps (docs/selection.md,
/// "Serving").
#[derive(Debug, PartialEq, Eq)]
enum Alias {
    /// These bytes.
    Bytes(Vec<u8>),
    /// The text, once decoded (by GTK and Xlib): `COMPOUND_TEXT` and `TEXT`, whose encoding is
    /// GTK's to judge, not ours.
    Decoded,
    /// It must not be offered.
    Not,
}

fn alias(name: &str, text: &str) -> Alias {
    let latin1 =
        || text.chars().map(|c| u8::try_from(u32::from(c)).ok()).collect::<Option<Vec<u8>>>();
    match name {
        "UTF8_STRING" | "text/plain;charset=utf-8" => Alias::Bytes(text.as_bytes().to_vec()),
        "STRING" => latin1().map_or(Alias::Not, Alias::Bytes),
        "text/plain" if text.is_ascii() => Alias::Bytes(text.as_bytes().to_vec()),
        "COMPOUND_TEXT" | "TEXT" => Alias::Decoded,
        _ => Alias::Not,
    }
}

fn newest(home: &Home) -> Option<Summary> {
    match ask(home, Request::List { offset: 0, limit: 1 }) {
        Response::List { entries, .. } => entries.into_iter().next(),
        other => panic!("not a list: {other:?}"),
    }
}

// ---- Payloads.

struct Files {
    dir: PathBuf,
}

impl Files {
    fn new(home: &Home) -> Self {
        let dir = home.root.join("payloads");
        ok(std::fs::create_dir_all(&dir));
        Files { dir }
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.dir.join(name);
        ok(std::fs::write(&path, bytes));
        path
    }
}

/// Text of about `len` bytes, every line different, with characters outside Latin-1.
fn text(len: usize) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut s = String::new();
    let mut n = 0u32;
    while s.len() < len {
        let _ = writeln!(s, "line {n}: héllo wörld ✓ ☃");
        n += 1;
    }
    let mut end = len.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s.into_bytes()
}

/// A PNG of `width`×`height` pixels of noise (incompressible, so the encoded size follows the
/// pixel count), stored without compression: enough PNG to be a real image without an encoder
/// dependency.
fn png(width: u32, height: u32) -> Vec<u8> {
    fn crc(data: &[u8]) -> u32 {
        let mut reg = 0xffff_ffffu32;
        for &byte in data {
            reg ^= u32::from(byte);
            for _ in 0..8 {
                reg = if reg & 1 == 1 { 0xedb8_8320 ^ (reg >> 1) } else { reg >> 1 };
            }
        }
        !reg
    }
    fn chunk(out: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
        out.extend_from_slice(&u32::try_from(data.len()).unwrap_or(0).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc(&body).to_be_bytes());
    }
    let mut pixels = Vec::new();
    let mut seed = 0x1234_5678u32;
    for _ in 0..height {
        let mut row = vec![0u8]; // no filter
        for _ in 0..width * 3 {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            row.push(seed.to_be_bytes()[1]);
        }
        pixels.extend_from_slice(&row);
    }
    // zlib: stored deflate blocks of at most 65535 bytes, then Adler-32.
    let mut zlib = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = pixels.chunks(65_535).collect();
    for (i, block) in blocks.iter().enumerate() {
        zlib.push(u8::from(i + 1 == blocks.len()));
        let len = u16::try_from(block.len()).unwrap_or(0);
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    let (mut low, mut high) = (1u32, 0u32);
    for &byte in &pixels {
        low = (low + u32::from(byte)) % 65_521;
        high = (high + low) % 65_521;
    }
    zlib.extend_from_slice(&((high << 16) | low).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::new();
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit RGB
    chunk(&mut out, *b"IHDR", &header);
    chunk(&mut out, *b"IDAT", &zlib);
    chunk(&mut out, *b"IEND", &[]);
    out
}

// ---- The matrix.

/// One row: `kit` offers `offer`; checks the three properties above and returns the bytes kept.
fn row(x: &Xvfb, home: &Home, kit: Kit, offer: Offer<'_>, what: &str) -> BTreeMap<String, Vec<u8>> {
    let before = newest(home).map(|s| s.id);
    let source = Source::start(kit, x, home, offer);
    let offered = targets(x).unwrap_or_default();
    let wanted = canonical(&offered);
    assert!(!wanted.is_empty(), "{what}: nothing canonical offered: {offered:?}");
    // The daemon's capture first: an owner such as xclip serves one INCR transfer at a time, so a
    // paste during a large capture fails for a reason that is not the daemon's.
    settle(home, before, what);
    // The truth: what the source itself serves, for every target it offers (a toolkit's
    // SAVE_TARGETS on quit has the daemon keep them all). Not the text names served from the
    // kept text: the daemon never fetches them beside it, and GTK 3 hangs rendering a large
    // non-ASCII text as `text/plain`.
    let has_text = wanted.iter().any(|k| TEXT.contains(&k.as_str()));
    let mut truth = BTreeMap::new();
    for t in offered.iter().filter(|t| !NEVER.contains(&t.as_str())) {
        if has_text && !wanted.contains(t) && alias(t, "") != Alias::Not {
            continue;
        }
        truth.extend(paste(Kit::Xclip, x, home, t).map(|b| (t.clone(), b)));
    }
    // A canonical target its owner refuses (GTK 3's `text/ico` for an image too big for ICO) is
    // not kept.
    let kept: Vec<String> = wanted.into_iter().filter(|t| truth.contains_key(t)).collect();
    assert!(!kept.is_empty(), "{what}: the source served nothing canonical");
    for t in &kept {
        let len = truth.get(t).map_or(0, Vec::len);
        for sink in sinks_for(t, len).filter(|&sink| !broken_pair(kit, sink, t, &offered, len)) {
            assert_eq!(
                paste(sink, x, home, t).as_deref().map(<[u8]>::len),
                truth.get(t).map(Vec::len),
                "{what}: {sink:?} pasting {t} from the live source"
            );
        }
    }
    let size: u64 = kept.iter().filter_map(|t| truth.get(t)).map(|b| b.len() as u64).sum();
    eventually(&format!("{what}: the daemon to keep {kept:?} ({size} bytes)"), || {
        newest(home).is_some_and(|s| Some(s.id) != before && s.size == size)
    });
    let took = source.quit();
    assert!(took < Duration::from_secs(5), "{what}: {kit:?} took {took:?} to quit");
    let mut first = true;
    for t in &kept {
        for sink in sinks_for(t, truth.get(t).map_or(0, Vec::len)) {
            let want = truth.get(t).cloned();
            if first {
                // The daemon takes the clipboard over once it sees the owner gone.
                eventually(&format!("{what}: the daemon serving {t}"), || {
                    paste(Kit::Xclip, x, home, t) == want
                });
                first = false;
            }
            let got = paste(sink, x, home, t);
            assert!(
                got == want,
                "{what}: {sink:?} pasting {t} after the source quit: {} bytes, want {}",
                got.map_or(0, |g| g.len()),
                want.map_or(0, |w| w.len())
            );
        }
    }
    offered_after_quit(x, home, what, &kept, &truth);
    truth
}

/// After the source quit, every target the daemon offers pastes as it must: what was kept as it
/// was, what `SAVE_TARGETS` saved as the source served it, and the text's aliases by their rules;
/// and nothing else is offered.
fn offered_after_quit(
    x: &Xvfb,
    home: &Home,
    what: &str,
    kept: &[String],
    truth: &BTreeMap<String, Vec<u8>>,
) {
    let after = targets(x).unwrap_or_default();
    let text =
        kept.iter().find(|t| TEXT.contains(&t.as_str())).and_then(|t| truth.get(t)).map(|b| {
            match String::from_utf8(b.clone()) {
                Ok(s) => s,
                Err(e) => e.into_bytes().iter().map(|&c| char::from(c)).collect(), // STRING is Latin-1
            }
        });
    for t in after.iter().filter(|t| !matches!(t.as_str(), "TARGETS" | "TIMESTAMP" | "MULTIPLE")) {
        if kept.contains(t) {
            continue;
        }
        // Saved from the source through `SAVE_TARGETS`: a rendering of its own, taken at quit.
        // Qt renders `image/cur` and `image/ico` differently each time it is asked, so it is
        // compared by length, and must be the same on every read from the daemon.
        let served = paste(Kit::Xclip, x, home, t);
        if let (Some(want), Some(got)) = (truth.get(t), &served)
            && got.len() == want.len()
        {
            assert_eq!(paste(Kit::Xclip, x, home, t).as_ref(), Some(got), "{what}: {t} changes");
            continue;
        }
        match text.as_deref().map_or(Alias::Not, |s| alias(t, s)) {
            Alias::Bytes(want) => {
                assert_eq!(paste(Kit::Xclip, x, home, t), Some(want), "{what}: alias {t}");
            }
            Alias::Decoded => {
                let decoded = toolkit(x, home, "clip-gtk3").args(["paste-text", t]).output();
                let decoded = ok(decoded);
                assert!(decoded.status.success(), "{what}: GTK cannot read alias {t}");
                assert_eq!(
                    String::from_utf8_lossy(&decoded.stdout),
                    text.as_deref().unwrap_or_default(),
                    "{what}: alias {t}, decoded"
                );
            }
            Alias::Not => panic!(
                "{what}: offers {t}, which it does not hold: {after:?}; the source served {:?} \
                 bytes of it, the daemon serves {:?}",
                truth.get(t).map(Vec::len),
                paste(Kit::Xclip, x, home, t).map(|b| b.len())
            ),
        }
    }
    if let Some(s) = &text {
        for name in [
            "UTF8_STRING",
            "text/plain;charset=utf-8",
            "STRING",
            "text/plain",
            "COMPOUND_TEXT",
            "TEXT",
        ] {
            let offered = after.iter().any(|t| t == name);
            let held = truth.contains_key(name);
            assert!(
                offered == (alias(name, s) != Alias::Not) || (offered && held),
                "{what}: whether {name} is offered"
            );
        }
    }
}

/// Waits for a new entry whose size has stopped changing: the daemon's capture is over.
fn settle(home: &Home, before: Option<u64>, what: &str) {
    let mut last = None;
    eventually(&format!("{what}: the daemon's capture to finish"), || {
        let now = newest(home).filter(|s| Some(s.id) != before).map(|s| s.size);
        let done = now.is_some() && now == last;
        last = now;
        if !done {
            std::thread::sleep(Duration::from_millis(250));
        }
        done
    });
}

struct Rig {
    x: Xvfb,
    home: Home,
    files: Files,
    _daemon: Daemon,
}

fn rig() -> Rig {
    let _ = clients();
    let x = Xvfb::start();
    let home = Home::new();
    let daemon = Daemon::start(&x, &home);
    let files = Files::new(&home);
    Rig { x, home, files, _daemon: daemon }
}

/// More than any client's largest single property here (16 MiB with BIG-REQUESTS), so it
/// crosses as INCR both ways.
const LARGE: usize = 20 << 20;
/// The largest GTK 4 can own: past the server's request size it fails (docs/conformance.md).
/// Still far past the daemon's INCR threshold, so the daemon serves it as INCR.
const GTK4_LARGE: usize = 12 << 20;
/// The most text GTK 4 serves through its own string serializer: it fails past 256 KiB.
const GTK4_TEXT: usize = 200_000;
/// The most xsel reads reliably: past it, it can fail allocating an INCR transfer, and leave
/// the owner stuck in it (docs/conformance.md).
const XSEL_MAX: usize = 1 << 20;

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
