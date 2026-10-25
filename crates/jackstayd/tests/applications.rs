//! The conformance matrix's application tier (docs/conformance.md): the owner's applications
//! copying and pasting through the real daemon on Xvfb, each driven as a person drives it (keys,
//! clicks, its context menu), `LibreOffice` through UNO.
//!
//! As a source, an application's copy is a row of the core tier's matrix ([`copied`]): the core
//! tier's clients paste what it serves, the daemon keeps the canonical targets, and once the
//! application has quit its own way every client pastes the same bytes from the daemon. As a
//! sink, it pastes what the daemon kept from a copy whose owner is gone, and what it pasted is
//! read back from the application itself.

#[path = "conformance/matrix.rs"]
mod matrix;
mod support;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use jackstay_proto::control::{Request, Response};
use jackstayd::client::Client;
use matrix::{
    Kit, LARGE, Offer, Owner, Rig, Source, copied, newest, owned_by_other, png, rgba, rig_on,
    settle, text,
};
use support::{Xvfb, ask, eventually, ok};

/// Room for a browser's window and its menus.
const SCREEN: &str = "1280x1024x24";

// Where the image puts them (Dockerfile).
const FIREFOX: &str = "/opt/apps/firefox/firefox";
const CHROME: &str = "/opt/apps/chrome-linux64/chrome";
const VSCODE: &str = "/opt/apps/VSCode-linux-x64/code";

/// Text that is not Latin-1, across lines, ending in a newline (a terminal passes on only whole
/// lines).
const LINES: &str = "pästed ☃ by an application\nsecond line ✓\n";

fn rig() -> Rig {
    rig_on(Xvfb::with_screen(SCREEN))
}

/// How many of these tests run at once. Each application holds inotify instances (VS Code about
/// 10, a browser 3), of the 128 a user may hold (Ubuntu's default), counted across the host: a
/// desktop can hold all but a dozen, and past the limit VS Code may fail to start.
const AT_ONCE: usize = 1;

static RUNNING: (Mutex<usize>, std::sync::Condvar) = (Mutex::new(0), std::sync::Condvar::new());

/// A test's turn to run its applications, until dropped.
struct Turn;

fn turn() -> Turn {
    let (running, freed) = &RUNNING;
    let mut n = running.lock().unwrap_or_else(PoisonError::into_inner);
    while *n >= AT_ONCE {
        n = freed.wait(n).unwrap_or_else(PoisonError::into_inner);
    }
    *n += 1;
    Turn
}

impl Drop for Turn {
    fn drop(&mut self) {
        let (running, freed) = &RUNNING;
        *running.lock().unwrap_or_else(PoisonError::into_inner) -= 1;
        freed.notify_one();
    }
}

// ---- Running applications.

/// How an application quits its own way.
enum Quit {
    Keys(&'static str),
    /// `LibreOffice`'s own exit, asked through UNO on this pipe.
    Office(String),
}

/// A running application, in a process group of its own, so that nothing it started outlives
/// the test.
struct App {
    name: &'static str,
    child: Child,
    display: String,
    quit: Quit,
    /// The daemon's control socket.
    control: PathBuf,
}

impl App {
    fn start(r: &Rig, name: &'static str, program: &str, quit: Quit, args: &[&str]) -> Self {
        let home = r.home.root.join("app-home");
        // Short, under any tree: VS Code puts its sockets here, and a socket's path must fit in
        // 108 bytes.
        let runtime = std::env::temp_dir().join(format!("jackstay-app-{}", std::process::id()));
        ok(std::fs::create_dir_all(&runtime));
        ok(std::fs::set_permissions(&runtime, std::os::unix::fs::PermissionsExt::from_mode(0o700)));
        let log = ok(std::fs::File::options()
            .create(true)
            .append(true)
            .open(r.home.root.join("applications.log")));
        let child = ok(r
            .x
            .command(program)
            .args(args)
            .env("HOME", &home)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_CACHE_HOME", home.join("cache"))
            .env("LANG", "C.UTF-8")
            .env("GDK_BACKEND", "x11")
            .env("NO_AT_BRIDGE", "1")
            .env("GTK_A11Y", "none")
            .env("SAL_USE_VCLPLUGIN", "gtk3")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove("WAYLAND_DISPLAY")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(ok(log.try_clone()))
            .stderr(log)
            .spawn());
        App { name, child, display: r.x.display.clone(), quit, control: r.home.socket() }
    }

    fn xdotool(&self, args: &[&str]) {
        xdotool(&self.display, args);
    }

    fn daemon_serves(&self) -> bool {
        let status = Client::connect(&self.control, Duration::from_secs(5))
            .and_then(|mut c| c.ask(&Request::Status));
        matches!(status, Ok(Response::Status(s)) if s.owns_clipboard)
    }
}

impl Owner for App {
    fn quit(mut self) -> Duration {
        let began = Instant::now();
        match &self.quit {
            Quit::Keys(keys) => self.xdotool(&["key", keys]),
            // Its answer may never come: LibreOffice is going.
            Quit::Office(pipe) => drop(office(&self.display, pipe, &["quit"])),
        }
        // Gone as far as the clipboard goes once the daemon serves in its place: a process can
        // linger after its windows close (Firefox, under load, for more than 30 s), and the
        // group is killed when the test is done with it.
        let deadline = began + Duration::from_secs(30);
        while ok(self.child.try_wait()).is_none() && !self.daemon_serves() {
            assert!(Instant::now() < deadline, "{} did not quit", self.name);
            std::thread::sleep(Duration::from_millis(10));
        }
        began.elapsed()
    }

    /// Less than GTK 3's 10 s wait on the manager (docs/selection.md, "`CLIPBOARD_MANAGER`"): the
    /// daemon answered within its 4 s budget, and the application's own exit took the rest.
    /// `LibreOffice` renders every format it offers when asked, which for a large image takes
    /// the whole budget.
    fn quits_within(&self) -> Duration {
        Duration::from_secs(9)
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let group = format!("-{}", self.child.id());
        let _ = Command::new("kill").args(["-KILL", "--", &group]).stderr(Stdio::null()).status();
        let _ = self.child.wait();
    }
}

#[track_caller]
fn xdotool(display: &str, args: &[&str]) {
    let status = ok(Command::new("xdotool").args(args).env("DISPLAY", display).status());
    assert!(status.success(), "xdotool {args:?}");
}

/// Waits up to `seconds` for a visible window matching `pattern` (`by` its `--name` or `--pid`).
fn find(r: &Rig, by: &str, pattern: &str, seconds: u32) -> Option<String> {
    let out = ok(r
        .x
        .command("timeout")
        .arg(seconds.to_string())
        .args(["xdotool", "search", "--sync", "--onlyvisible", by, pattern])
        .stderr(Stdio::null())
        .output());
    let found = String::from_utf8_lossy(&out.stdout);
    found.lines().last().map(str::to_owned)
}

/// Waits for a visible window matching `pattern`, and returns it.
fn window(r: &Rig, what: &str, by: &str, pattern: &str) -> String {
    let id = find(r, by, pattern, 60);
    id.unwrap_or_else(|| panic!("{what}: no window {pattern}"))
}

/// Does `attempt` until `done`, giving each attempt `wait`: a key sent before a window, an
/// editor or a menu is ready is lost, not queued.
fn until(what: &str, wait: Duration, mut attempt: impl FnMut(), mut done: impl FnMut() -> bool) {
    for _ in 0..5 {
        attempt();
        let deadline = Instant::now() + wait;
        while Instant::now() < deadline {
            if done() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    panic!("{what}: never happened");
}

/// Does `copy` until something other than the daemon owns the clipboard.
fn until_copied(r: &Rig, what: &str, copy: impl FnMut()) {
    until(what, Duration::from_secs(3), copy, || owned_by_other(&r.x, &r.home));
}

/// Long enough for the largest paste here to land: an attempt that has not landed by then is
/// taken to be lost, and made again.
const PASTING: Duration = Duration::from_secs(15);

/// Puts `offer` on the clipboard from `kit`, waits for the daemon to keep it, and quits `kit`:
/// what an application pastes next comes from the daemon.
fn on_clipboard(r: &Rig, kit: Kit, offer: Offer<'_>, what: &str) {
    let before = newest(&r.home).map(|s| s.id);
    let source = Source::start(kit, &r.x, &r.home, offer);
    settle(&r.home, before, what);
    let _ = source.quit();
    eventually(
        &format!("{what}: the daemon serving it"),
        || matches!(ask(&r.home, Request::Status), Response::Status(s) if s.owns_clipboard),
    );
}

/// A PNG's width and height, from its header.
fn dimensions(png: &[u8]) -> Option<(u32, u32)> {
    let header = png.get(16..24)?;
    let (w, h) = header.split_at(4);
    Some((u32::from_be_bytes(w.try_into().ok()?), u32::from_be_bytes(h.try_into().ok()?)))
}

// ---- The browsers' pages, served by the test.

/// Serves the payload directory to the browsers, and takes what their paste page sends back.
struct Web {
    port: u16,
    got: Arc<Mutex<Got>>,
}

/// What the paste page sent: each thing's paste (numbered from 1), name and bytes.
type Got = Vec<(u64, String, Vec<u8>)>;

impl Web {
    fn start(dir: &Path) -> Self {
        let listener = ok(TcpListener::bind("127.0.0.1:0"));
        let port = ok(listener.local_addr()).port();
        let got = Arc::new(Mutex::new(Vec::new()));
        let (dir, sink) = (dir.to_owned(), Arc::clone(&got));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (dir, sink) = (dir.clone(), Arc::clone(&sink));
                std::thread::spawn(move || serve(&stream, &dir, &sink));
            }
        });
        Web { port, got }
    }

    fn url(&self, page: &str) -> String {
        format!("http://127.0.0.1:{}/{page}", self.port)
    }

    /// The paste the page numbered `n`, if it has sent all of it.
    fn paste(&self, n: u64) -> Option<BTreeMap<String, Vec<u8>>> {
        let got = self.got.lock().unwrap_or_else(PoisonError::into_inner);
        let ours = || got.iter().filter(move |(k, _, _)| *k == n);
        ours()
            .any(|(_, name, _)| name == "done")
            .then(|| ours().map(|(_, name, body)| (name.clone(), body.clone())).collect())
    }

    /// Whether the page has begun sending a paste numbered `n`.
    fn begun(&self, n: u64) -> bool {
        let got = self.got.lock().unwrap_or_else(PoisonError::into_inner);
        got.iter().any(|(k, _, _)| *k == n)
    }
}

/// One HTTP/1.1 exchange: a GET for a file in `dir`, or the paste page's POST of one thing.
fn serve(stream: &TcpStream, dir: &Path, got: &Mutex<Got>) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let mut words = line.split_whitespace();
    let (method, path) = (words.next().unwrap_or_default(), words.next().unwrap_or_default());
    let (mut length, mut name) = (0usize, String::new());
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).unwrap_or(0) == 0 {
            return;
        }
        let Some((key, value)) = header.trim_end().split_once(':') else { break };
        match key.to_ascii_lowercase().as_str() {
            "content-length" => length = value.trim().parse().unwrap_or(0),
            "x-name" => value.trim().clone_into(&mut name),
            _ => {}
        }
    }
    let mut body = vec![0; length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let file = path.trim_start_matches('/');
    let (status, content) = if method == "POST" {
        let (n, name) = name.split_once(' ').unwrap_or_default();
        let entry = (n.parse().unwrap_or(0), name.to_owned(), body);
        got.lock().unwrap_or_else(PoisonError::into_inner).push(entry);
        ("200 OK", Vec::new())
    } else if let Some(Ok(bytes)) =
        (!file.contains('/') && !file.starts_with('.')).then(|| std::fs::read(dir.join(file)))
    {
        ("200 OK", bytes)
    } else {
        ("404 Not Found", Vec::new())
    };
    let png = Path::new(file).extension().is_some_and(|e| e == "png");
    let kind = if png { "image/png" } else { "text/html; charset=utf-8" };
    let mut out = stream;
    let _ = write!(
        out,
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        content.len()
    );
    let _ = out.write_all(&content);
}

/// A page whose title becomes "ready" once `script` (a module: it may `await`) has run.
fn page(r: &Rig, name: &str, body: &str, script: &str) {
    let html = format!(
        "<!doctype html><html><head><meta charset=utf-8>\
         <style>html,body{{margin:0;height:100%}}.all{{position:fixed;inset:0;display:block;\
         width:100%;height:100%}}</style></head><body>{body}<script type=module>{script}\
         document.title='ready';</script></body></html>"
    );
    r.files.write(name, html.as_bytes());
}

/// Selects all of `#t`.
const SELECT: &str = "const all = document.createRange();\
    all.selectNodeContents(document.getElementById('t'));\
    getSelection().removeAllRanges(); getSelection().addRange(all);";

/// A page with `html` selected, for Ctrl+C.
fn selection_page(r: &Rig, name: &str, html: &str) {
    page(r, name, &format!("<div id=t>{html}</div>"), SELECT);
}

/// A page with the text file `file` selected, preformatted. Fetched by a script, not parsed:
/// Chrome takes minutes to parse 20 MiB of HTML.
fn large_selection_page(r: &Rig, name: &str, file: &str) {
    let fetch = format!(
        "document.getElementById('t').textContent = await (await fetch('{file}')).text();{SELECT}"
    );
    page(r, name, "<pre id=t></pre>", &fetch);
}

/// A page that is one image, for its context menu's Copy Image.
fn image_page(r: &Rig, name: &str, image: &str) {
    page(
        r,
        name,
        &format!("<img class=all id=i src={image}>"),
        "await document.getElementById('i').decode();",
    );
}

/// A page that is one link, for its context menu's Copy Link.
fn link_page(r: &Rig, name: &str, href: &str) {
    page(r, name, &format!("<a class=all href=\"{href}\">a link ✓</a>"), "");
}

/// The page every browser pastes into: it sends back each type a paste holds, and each image as
/// its pixels (RGBA, read from a canvas), each under the paste's number.
const PASTE_PAGE: &str = "
const d = document.getElementById('d');
d.focus();
let pastes = 0;
d.addEventListener('paste', async e => {
  const n = ++pastes;
  const send = (name, body) =>
    fetch('/got', {method: 'POST', headers: {'X-Name': `${n} ${name}`}, body});
  e.preventDefault();
  // The event's data is gone once the handler first waits: read it all now.
  const c = e.clipboardData, types = Array.from(c.types), files = Array.from(c.files);
  const data = types.filter(t => t !== 'Files').map(t => [t, c.getData(t)]);
  await send('types', types.join('\\n'));
  for (const [t, v] of data) await send(t, v);
  for (const f of files) {
    const b = await createImageBitmap(f);
    const canvas = new OffscreenCanvas(b.width, b.height);
    const g = canvas.getContext('2d');
    g.drawImage(b, 0, 0);
    const pixels = g.getImageData(0, 0, b.width, b.height).data;
    await send(`file ${f.type} ${b.width}x${b.height}`, pixels);
  }
  await send('done', '');
});
";

fn paste_page(r: &Rig) {
    page(r, "paste.html", "<div class=all id=d contenteditable></div>", PASTE_PAGE);
}

// ---- The browsers.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Browser {
    Firefox,
    Chrome,
}

impl Browser {
    fn start(self, r: &Rig, url: &str) -> (App, String) {
        let profile = tempdir(r, "profile");
        let app = match self {
            Browser::Firefox => {
                ok(std::fs::write(profile.join("user.js"), FIREFOX_PREFS));
                App::start(
                    r,
                    "Firefox",
                    FIREFOX,
                    Quit::Keys("ctrl+q"),
                    &["--no-remote", "--new-instance", "--profile", path(&profile), url],
                )
            }
            Browser::Chrome => App::start(
                r,
                "Chrome",
                CHROME,
                Quit::Keys("ctrl+shift+w"),
                &[
                    &format!("--user-data-dir={}", profile.display()),
                    "--no-first-run",
                    "--no-default-browser-check",
                    // Its sandbox needs user namespaces, which the container does not give.
                    "--no-sandbox",
                    "--disable-gpu",
                    "--password-store=basic",
                    "--ozone-platform=x11",
                    "--window-position=0,0",
                    "--window-size=1000,800",
                    url,
                ],
            ),
        };
        let w = window(r, &format!("{self:?}"), "--name", "^ready");
        (app, w)
    }

    /// The context menu's Copy Image (or Copy Link, `link`) on the page's one element.
    fn copy_from_menu(self, app: &App, w: &str, link: bool) {
        app.xdotool(&["key", "Escape"]);
        app.xdotool(&["mousemove", "--window", w, "400", "400", "click", "3"]);
        std::thread::sleep(Duration::from_millis(700));
        match (self, link) {
            // Its access keys.
            (Browser::Firefox, false) => app.xdotool(&["key", "y"]),
            (Browser::Firefox, true) => app.xdotool(&["key", "l"]),
            // No access keys: the item by its place in 155's menus, "Copy image" third from
            // the top, "Copy link address" third from the bottom.
            (Browser::Chrome, false) => app.xdotool(&["key", "Down", "Down", "Down", "Return"]),
            (Browser::Chrome, true) => app.xdotool(&["key", "Up", "Up", "Up", "Return"]),
        }
    }
}

const FIREFOX_PREFS: &str = r#"
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.startup.homepage_override.mstone", "ignore");
user_pref("browser.aboutwelcome.enabled", false);
user_pref("browser.preonboarding.enabled", false);
user_pref("termsofuse.bypassNotification", true);
user_pref("termsofuse.acceptedVersion", 4);
user_pref("termsofuse.acceptedDate", "1700000000000");
user_pref("datareporting.policy.dataSubmissionEnabled", false);
user_pref("toolkit.telemetry.reportingpolicy.firstRun", false);
user_pref("app.update.disabledForTesting", true);
user_pref("browser.warnOnQuitShortcut", false);
user_pref("browser.tabs.warnOnClose", false);
user_pref("browser.sessionstore.resume_from_crash", false);
"#;

fn tempdir(r: &Rig, kind: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = r.home.root.join(format!("{kind}-{n}"));
    ok(std::fs::create_dir_all(&dir));
    dir
}

fn path(p: &Path) -> &str {
    p.to_str().unwrap_or_default()
}

/// The browser's copies, each a row of the matrix.
fn browser_copies(b: Browser, r: &Rig, web: &Web) {
    let name = format!("{b:?}");
    selection_page(r, "selection.html", "héllo <b>wörld</b> ✓ &amp; more");
    let large = String::from_utf8_lossy(&text(LARGE)).into_owned();
    r.files.write("large.txt", large.as_bytes());
    large_selection_page(r, "large.html", "large.txt");
    r.files.write("small.png", &png(64, 48));
    r.files.write("large.png", &png(2600, 2600));
    image_page(r, "small-image.html", "small.png");
    image_page(r, "large-image.html", "large.png");
    let href = "https://example.com/a%20b?c=d&e=%C3%A9";
    link_page(r, "link.html", &href.replace('&', "&amp;"));

    for (page, what, check) in [
        ("selection.html", "selected text with markup", Some("héllo wörld ✓ & more")),
        ("large.html", "20 MiB of selected text", None),
    ] {
        let what = format!("{name} {what}");
        let kept = copied(&r.x, &r.home, None, &[], &what, || {
            let (app, w) = b.start(r, &web.url(page));
            until_copied(r, &what, || {
                app.xdotool(&["mousemove", "--window", &w, "400", "400", "key", "ctrl+c"]);
            });
            app
        });
        let text = kept.get("UTF8_STRING").map(|t| String::from_utf8_lossy(t).into_owned());
        if let Some(want) = check {
            assert_eq!(text.as_deref(), Some(want), "{what}");
        } else {
            assert_eq!(text.map(|t| t.trim_end().len()), Some(large.trim_end().len()), "{what}");
        }
    }
    for (page, what, size) in [
        ("small-image.html", "Copy Image", (64, 48)),
        ("large-image.html", "large Copy Image", (2600, 2600)),
    ] {
        let what = format!("{name} {what}");
        let kept = copied(&r.x, &r.home, None, &[], &what, || {
            let (app, w) = b.start(r, &web.url(page));
            until_copied(r, &what, || b.copy_from_menu(&app, &w, false));
            app
        });
        assert_eq!(kept.get("image/png").and_then(|p| dimensions(p)), Some(size), "{what}");
    }
    let what = format!("{name} Copy Link");
    let kept = copied(&r.x, &r.home, None, &[], &what, || {
        let (app, w) = b.start(r, &web.url("link.html"));
        until_copied(r, &what, || b.copy_from_menu(&app, &w, true));
        app
    });
    assert_eq!(kept.get("UTF8_STRING").map(Vec::as_slice), Some(href.as_bytes()), "{what}");
}

/// The browser pastes, into one page, each copy the daemon kept once its owner had gone.
fn browser_pastes(b: Browser, r: &Rig, web: &Web) {
    let name = format!("{b:?}");
    paste_page(r);
    let small = r.files.write("paste.txt", LINES.as_bytes());
    let large_text = text(LARGE);
    let large = r.files.write("paste-large.txt", &large_text);
    let plain = r.files.write("paste-plain.txt", b"bold & more");
    let html = r.files.write("paste.html.part", b"<b>bold</b> &amp; <i>more</i>");
    let image = r.files.write("paste.png", &png(64, 48));
    let big_image = r.files.write("paste-large.png", &png(2600, 2600));
    let (app, w) = b.start(r, &web.url("paste.html"));
    let pastes = std::cell::Cell::new(0);
    let paste = |kit: Kit, offer: Offer<'_>, what: &str| {
        let what = format!("{name} pasting {what}");
        on_clipboard(r, kit, offer, &what);
        pastes.set(pastes.get() + 1);
        let n = pastes.get();
        // Made again only while no paste has begun: a key that reached the page pasted, and a
        // second paste would follow it.
        until(
            &what,
            Duration::from_secs(3),
            || {
                app.xdotool(&["mousemove", "--window", &w, "400", "400", "click", "1"]);
                app.xdotool(&["key", "ctrl+v"]);
            },
            || web.begun(n),
        );
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(got) = web.paste(n) {
                break got;
            }
            assert!(Instant::now() < deadline, "{what}: the page never had all of it");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let got = paste(Kit::Xclip, &[("UTF8_STRING", &small)], "text");
    assert_eq!(got.get("text/plain").map(Vec::as_slice), Some(LINES.as_bytes()), "{name} text");
    let got = paste(Kit::Xclip, &[("UTF8_STRING", &large)], "20 MiB of text");
    assert!(got.get("text/plain") == Some(&large_text), "{name}: 20 MiB of text");
    let got = paste(Kit::Gtk3, &[("text", &plain), ("text/html", &html)], "text and html");
    let pasted_html = got.get("text/html").map(|h| String::from_utf8_lossy(h).into_owned());
    assert!(
        pasted_html.as_deref().is_some_and(|h| h.contains("<b>bold</b> &amp; <i>more</i>")),
        "{name} html: {pasted_html:?}"
    );
    for (file, (width, height), what) in
        [(&image, (64, 48), "an image"), (&big_image, (2600, 2600), "a large image")]
    {
        let got = paste(Kit::Xclip, &[("image/png", file)], what);
        let key = format!("file image/png {width}x{height}");
        assert!(
            got.get(&key) == Some(&rgba(width, height)),
            "{name} pasting {what}: got {:?}",
            got.keys().collect::<Vec<_>>()
        );
    }
    drop(app);
}

#[test]
fn firefox() {
    let _turn = turn();
    let r = rig();
    let web = Web::start(&r.files.dir);
    browser_copies(Browser::Firefox, &r, &web);
    browser_pastes(Browser::Firefox, &r, &web);
}

#[test]
fn chrome() {
    let _turn = turn();
    let r = rig();
    let web = Web::start(&r.files.dir);
    browser_copies(Browser::Chrome, &r, &web);
    browser_pastes(Browser::Chrome, &r, &web);
}

// ---- VS Code (Electron).

const VSCODE_SETTINGS: &str = r#"{
  "workbench.startupEditor": "none",
  "workbench.tips.enabled": false,
  "telemetry.telemetryLevel": "off",
  "update.mode": "none",
  "security.workspace.trust.enabled": false,
  "window.titleBarStyle": "native",
  "window.restoreWindows": "none",
  "files.hotExit": "off",
  "extensions.ignoreRecommendations": true
}"#;

fn vscode(r: &Rig, file: &Path) -> (App, String) {
    let data = tempdir(r, "vscode");
    let user = data.join("user");
    ok(std::fs::create_dir_all(user.join("User")));
    ok(std::fs::write(user.join("User/settings.json"), VSCODE_SETTINGS));
    let extensions = data.join("extensions");
    let app = App::start(
        r,
        "VS Code",
        VSCODE,
        Quit::Keys("ctrl+q"),
        &[
            // Its sandbox needs user namespaces, which the container does not give.
            "--no-sandbox",
            "--disable-gpu",
            "--user-data-dir",
            path(&user),
            "--extensions-dir",
            path(&extensions),
            "--disable-extensions",
            "--skip-welcome",
            "--skip-release-notes",
            "--new-window",
            path(file),
        ],
    );
    let name = file.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let Some(w) = find(r, "--name", &format!("^{name} - Visual Studio Code"), 60) else {
        let log = std::fs::read_to_string(r.home.root.join("applications.log"));
        let starved = log.is_ok_and(|l| l.contains("inotify_init() failed: Too many open files"));
        assert!(
            !starved,
            "VS Code did not start: this user's inotify instances ran out (it takes about 10; \
             fs.inotify.max_user_instances, 128 on Ubuntu, counts the whole desktop's; see \
             docs/conformance.md)"
        );
        panic!("VS Code: no window for {name}");
    };
    (app, w)
}

#[test]
fn vscode_copies_and_pastes() {
    let _turn = turn();
    let r = rig();
    let code = "fn main() {\n    println!(\"héllo ✓\");\n}\n";
    let medium_text = text(1 << 20);
    for (name, bytes, what) in [
        ("code.rs", code.as_bytes(), "VS Code selected code"),
        ("medium.txt", &medium_text, "VS Code 1 MiB"),
    ] {
        let file = r.files.write(name, bytes);
        let kept = copied(&r.x, &r.home, None, &[], what, || {
            let (app, w) = vscode(&r, &file);
            // With nothing selected it copies the line: Ctrl+A before the editor is ready
            // copies one line. Made again until the whole file is on the clipboard.
            until(
                what,
                Duration::from_secs(3),
                || {
                    app.xdotool(&["mousemove", "--window", &w, "400", "300", "click", "1"]);
                    app.xdotool(&["key", "ctrl+a", "ctrl+c"]);
                },
                || {
                    owned_by_other(&r.x, &r.home)
                        && matrix::paste(Kit::Xclip, &r.x, &r.home, "UTF8_STRING").as_deref()
                            == Some(bytes)
                },
            );
            app
        });
        assert_eq!(kept.get("UTF8_STRING").map(Vec::as_slice), Some(bytes), "{what}");
    }

    let out = r.files.write("pasted.txt", b"");
    let (app, w) = vscode(&r, &out);
    for (bytes, what) in [(LINES.as_bytes(), "text"), (&medium_text[..], "1 MiB of text")] {
        let what = format!("VS Code pasting {what}");
        let file = r.files.write("paste.txt", bytes);
        on_clipboard(&r, Kit::Xclip, &[("UTF8_STRING", &file)], &what);
        until(
            &what,
            PASTING,
            || {
                app.xdotool(&["mousemove", "--window", &w, "400", "300", "click", "1"]);
                app.xdotool(&["key", "ctrl+a", "ctrl+v", "ctrl+s"]);
            },
            || std::fs::read(&out).is_ok_and(|b| b == bytes),
        );
    }
    drop(app);
}

// ---- LibreOffice (Writer, its GTK 3 plugin, as on Xubuntu).

/// What `LibreOffice` renders differently from one request to the next (docs/conformance.md):
/// its first export of a copy lacks styles that later ones have, its own document format
/// (under its public name and a name of the process's own) differs on every export, and its
/// link to the document is empty once the document is closing (as it is when `SAVE_TARGETS`
/// asks for it).
const OFFICE_RENDERS_ANEW: [&str; 6] = [
    "text/rtf",
    "text/richtext",
    "text/html",
    "application/x-openoffice-embed-source-xml",
    "application/x-libreoffice-internal-id-",
    "application/x-openoffice-link",
];

fn office(display: &str, pipe: &str, args: &[&str]) -> std::process::Output {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/conformance/office.py");
    ok(Command::new("timeout")
        .args(["60", "python3", "-I"])
        .arg(script)
        .arg(pipe)
        .args(args)
        .env("DISPLAY", display)
        .stderr(Stdio::inherit())
        .output())
}

fn libreoffice(r: &Rig) -> (App, String) {
    let profile = tempdir(r, "office");
    let pipe = format!(
        "jackstay-{}-{}",
        std::process::id(),
        profile.file_name().and_then(|n| n.to_str()).unwrap_or_default()
    );
    let app = App::start(
        r,
        "LibreOffice",
        "soffice",
        Quit::Office(pipe.clone()),
        &[
            &format!("-env:UserInstallation=file://{}", profile.display()),
            "--norestore",
            "--nologo",
            "--nodefault",
            &format!("--accept=pipe,name={pipe};urp;"),
        ],
    );
    (app, pipe)
}

#[test]
fn libreoffice_copies_and_pastes() {
    let _turn = turn();
    let r = rig();
    let lines = r.files.write("lines.txt", LINES.as_bytes());
    let image = r.files.write("small.png", &png(64, 48));
    let big_image = r.files.write("large.png", &png(2600, 2600));
    let what = "LibreOffice Writer text";
    let kept = copied(&r.x, &r.home, None, &OFFICE_RENDERS_ANEW, what, || {
        let (app, pipe) = libreoffice(&r);
        until_copied(&r, what, || {
            let _ = office(&r.x.display, &pipe, &["copy-text", path(&lines)]);
        });
        app
    });
    assert_eq!(
        kept.get("UTF8_STRING").map(|t| String::from_utf8_lossy(t).replace("\r\n", "\n")),
        Some(LINES.to_owned()),
        "{what}"
    );
    // What it renders anew is checked against what the daemon kept; that must hold the text.
    for t in ["text/html", "text/rtf", "text/richtext"] {
        let held = kept.get(t).map(|b| String::from_utf8_lossy(b).into_owned());
        assert!(held.is_some_and(|h| h.contains("second line")), "{what}: {t}");
    }
    for (file, size, what) in [
        (&image, (64, 48), "LibreOffice Writer image"),
        (&big_image, (2600, 2600), "LibreOffice Writer large image"),
    ] {
        let kept = copied(&r.x, &r.home, None, &[], what, || {
            let (app, pipe) = libreoffice(&r);
            until_copied(&r, what, || {
                let _ = office(&r.x.display, &pipe, &["copy-image", path(file)]);
            });
            app
        });
        assert_eq!(kept.get("image/png").and_then(|p| dimensions(p)), Some(size), "{what}");
    }

    let (app, pipe) = libreoffice(&r);
    let pasted = |what: &str| {
        let out = office(&r.x.display, &pipe, &["paste"]);
        assert!(out.status.success(), "{what}: office.py paste");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let what = "LibreOffice pasting text";
    on_clipboard(&r, Kit::Xclip, &[("UTF8_STRING", &lines)], what);
    // Its last line break ends its last paragraph; the document's text does not show it.
    let lines = LINES.strip_suffix('\n').unwrap_or(LINES);
    assert_eq!(pasted(what), format!("graphics 0 -\n{lines}"), "{what}");
    for (file, size, what) in [
        (&image, "64x48", "LibreOffice pasting an image"),
        (&big_image, "2600x2600", "LibreOffice pasting a large image"),
    ] {
        on_clipboard(&r, Kit::Xclip, &[("image/png", file)], what);
        let got = pasted(what);
        assert_eq!(got.lines().next(), Some(format!("graphics 1 {size}").as_str()), "{what}");
    }
    drop(app);
}

// ---- xfce4-terminal (VTE).

fn terminal(r: &Rig, script: &str) -> (App, String) {
    let app = App::start(
        r,
        "xfce4-terminal",
        "xfce4-terminal",
        // Return ends the shell's `read`, and the terminal closes with its shell.
        Quit::Keys("Return"),
        &[
            "--disable-server",
            "--hide-menubar",
            "--hide-scrollbar",
            "--hide-toolbar",
            "--geometry",
            "80x10+0+0",
            "-x",
            "sh",
            "-c",
            script,
        ],
    );
    let w = window(r, "xfce4-terminal", "--pid", &app.child.id().to_string());
    (app, w)
}

#[test]
fn xfce4_terminal_copies_and_pastes() {
    let _turn = turn();
    let r = rig();
    let shown = r.files.write("shown.txt", "héllo wörld ✓ from VTE\nnot selected\n".as_bytes());
    let what = "xfce4-terminal selected line";
    let kept = copied(&r.x, &r.home, None, &[], what, || {
        let (app, w) = terminal(&r, &format!("cat '{}'; read x", shown.display()));
        until_copied(&r, what, || {
            app.xdotool(&["mousemove", "--window", &w, "30", "8"]);
            app.xdotool(&["click", "--repeat", "3", "--delay", "80", "1"]);
            app.xdotool(&["key", "ctrl+shift+c"]);
        });
        app
    });
    assert_eq!(
        kept.get("UTF8_STRING").map(|t| String::from_utf8_lossy(t).into_owned()).as_deref(),
        Some("héllo wörld ✓ from VTE\n"),
        "{what}"
    );

    let out = r.files.write("terminal-out.txt", b"");
    let (app, w) = terminal(&r, &format!("stty -echo -icanon; exec cat >> '{}'", out.display()));
    let lines_64k: String = LINES.repeat((64 << 10) / LINES.len());
    for (want, what) in [(LINES, "text"), (lines_64k.as_str(), "64 KiB of text")] {
        let what = format!("xfce4-terminal pasting {what}");
        let file = r.files.write("paste.txt", want.as_bytes());
        on_clipboard(&r, Kit::Xclip, &[("UTF8_STRING", &file)], &what);
        until(
            &what,
            PASTING,
            || {
                ok(std::fs::write(&out, b""));
                app.xdotool(&["mousemove", "--window", &w, "30", "30", "key", "ctrl+shift+v"]);
                // It asks before pasting a line break; the test answers Paste, as a person would.
                if find(&r, "--name", "^Warning: Potentially Unsafe Paste", 10).is_some() {
                    app.xdotool(&["key", "alt+p"]);
                }
            },
            || std::fs::read(&out).is_ok_and(|b| b == want.as_bytes()),
        );
    }
    drop(app);
}
