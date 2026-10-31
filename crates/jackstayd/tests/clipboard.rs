//! Clipboard sharing between two daemons through the relay (docs/sharing.md, "Testing"): copies
//! made with `xclip` (or an owner of the test's own, where `xclip` cannot be slow) on one
//! machine's X server, pasted with `xclip` on the other's.

mod support;

use std::io::Write as _;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use jackstay_proto::control::{Kinds, Request, Response, Summary};
use support::link::{Faults, Machine, Rig};
use support::{Home, Xvfb, ask, copy, eventually, kill, ok, paste};
use x11rb::connection::Connection as _;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::wrapper::ConnectionExt as _;

/// What a version 6 client lists, newest first.
fn entries(home: &Home) -> Vec<Summary> {
    match ask(home, Request::Entries { offset: 0, limit: 100 }) {
        Response::Entries { entries, .. } => entries,
        other => panic!("not entries: {other:?}"),
    }
}

fn previews(home: &Home) -> Vec<String> {
    entries(home).into_iter().map(|e| e.preview).collect()
}

/// Bytes that are not text and do not repeat, so a truncated or wrong paste shows.
fn image(len: usize, seed: u8) -> Vec<u8> {
    let mut s = u32::from(seed) | 0x9e37_0000;
    (0..len)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            s.to_le_bytes()[0]
        })
        .collect()
}

fn copy_image(x: &Xvfb, bytes: &[u8]) -> Child {
    let mut child = ok(x
        .command("xclip")
        .args(["-quiet", "-selection", "clipboard", "-target", "image/png", "-i"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn());
    let mut stdin = child.stdin.take().unwrap_or_else(|| panic!("no stdin"));
    ok(stdin.write_all(bytes));
    drop(stdin);
    child
}

/// The clipboard's image, or `None` if the paste was refused (or took more than 10 s).
fn paste_image(x: &Xvfb) -> Option<Vec<u8>> {
    let out = ok(x
        .command("timeout")
        .args(["10", "xclip", "-selection", "clipboard", "-target", "image/png", "-o"])
        .stderr(Stdio::null())
        .output());
    out.status.success().then_some(out.stdout).filter(|b| !b.is_empty())
}

/// Waits until the newest entry on `m` holds an image from the other machine.
fn image_arrived(m: &Machine) {
    eventually("the image's announcement", || {
        entries(&m.home).first().is_some_and(|e| e.from_peer && e.kinds.0 & Kinds::IMAGE != 0)
    });
}

/// How many times `m`'s journal says `what`.
fn said(m: &Machine, what: &str) -> usize {
    let log = std::fs::read_to_string(m.home.root.join("daemon.log")).unwrap_or_default();
    log.lines().filter(|l| l.starts_with(what)).count()
}

#[test]
fn copies_on_either_machine_paste_on_the_other() {
    let rig = Rig::linked();
    let on_a = copy(&rig.a.x, "copied on a");
    eventually("a's copy pasted on b", || paste(&rig.b.x).as_deref() == Some("copied on a"));
    let first = entries(&rig.b.home);
    assert_eq!(
        first.first().map(|e| (e.preview.as_str(), e.from_peer)),
        Some(("copied on a", true))
    );
    // And back.
    let on_b = copy(&rig.b.x, "copied on b");
    eventually("b's copy pasted on a", || paste(&rig.a.x).as_deref() == Some("copied on b"));
    assert_eq!(previews(&rig.a.home), ["copied on b", "copied on a"]);
    // An image alone, too large to travel with its announcement: fetched, and pasted whole.
    let png = image(300 * 1024, 1);
    let picture = copy_image(&rig.a.x, &png);
    image_arrived(&rig.b);
    eventually("the image pasted on b", || paste_image(&rig.b.x).as_deref() == Some(&png[..]));
    for c in [on_a, on_b, picture] {
        kill(c);
    }
}

#[test]
fn a_large_image_pastes_while_it_arrives_and_never_in_part() {
    let rig = Rig::linked();
    // 16 MiB over a path with 5 ms each way: the fetch takes under a second.
    rig.relay.set(Faults { delay: Duration::from_millis(5), ..Faults::default() });
    let png = image(16 << 20, 2);
    let first = copy_image(&rig.a.x, &png);
    image_arrived(&rig.b);
    // Pasted at once: the paste waits for the fetch.
    assert_eq!(paste_image(&rig.b.x).map(|b| b.len()), Some(png.len()));
    assert!(paste_image(&rig.b.x).as_deref() == Some(&png[..]), "the image pasted whole");
    // The link going while the next one is fetched: the paste is refused, never cut short.
    rig.relay.set(Faults { delay: Duration::from_millis(50), ..Faults::default() });
    let other = image(16 << 20, 3);
    let second = copy_image(&rig.a.x, &other);
    eventually("the second image's announcement", || {
        entries(&rig.b.home).first().is_some_and(|e| e.from_peer && e.size < other.len() as u64)
    });
    rig.relay.set(Faults { blackhole: true, ..Faults::default() });
    let asked = Instant::now();
    assert_eq!(paste_image(&rig.b.x), None);
    assert!(asked.elapsed() < Duration::from_secs(6), "refused at its deadline");
    kill(first);
    kill(second);
}

/// An owner that takes the clipboard on `x` and answers each request after `slow`, as an office suite
/// can: offers `text` as `UTF8_STRING`. Runs until `stop`, answering what it was asked even after
/// it loses the clipboard, as applications do (a requestor left unanswered waits for ever). Says
/// on `asked` when the first request comes: the daemon has started capturing.
fn slow_owner(
    x: &Xvfb,
    text: &str,
    slow: Duration,
    stop: Arc<AtomicBool>,
    asked: mpsc::Sender<()>,
) -> JoinHandle<()> {
    let (conn, screen) = ok(x11rb::connect(Some(&x.display)));
    let text = text.as_bytes().to_vec();
    std::thread::spawn(move || {
        let root = conn.setup().roots[screen].root;
        let window = ok(conn.generate_id());
        ok(conn.create_window(
            0,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            xproto::WindowClass::INPUT_ONLY,
            0,
            &xproto::CreateWindowAux::new(),
        ));
        let atom = |name: &str| ok(ok(conn.intern_atom(false, name.as_bytes())).reply()).atom;
        let (clipboard, targets, utf8) = (atom("CLIPBOARD"), atom("TARGETS"), atom("UTF8_STRING"));
        ok(conn.set_selection_owner(window, clipboard, x11rb::CURRENT_TIME));
        ok(conn.flush());
        while !stop.load(Ordering::Relaxed) {
            let Some(event) = ok(conn.poll_for_event()) else {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            };
            if let Event::SelectionRequest(r) = event {
                let _ = asked.send(());
                std::thread::sleep(slow);
                let property = if r.target == targets {
                    let list = [targets, utf8];
                    ok(conn.change_property32(
                        xproto::PropMode::REPLACE,
                        r.requestor,
                        r.property,
                        xproto::AtomEnum::ATOM,
                        &list,
                    ));
                    r.property
                } else if r.target == utf8 {
                    ok(conn.change_property8(
                        xproto::PropMode::REPLACE,
                        r.requestor,
                        r.property,
                        utf8,
                        &text,
                    ));
                    r.property
                } else {
                    x11rb::NONE
                };
                let notify = xproto::SelectionNotifyEvent {
                    response_type: xproto::SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: r.time,
                    requestor: r.requestor,
                    selection: r.selection,
                    target: r.target,
                    property,
                };
                ok(conn.send_event(false, r.requestor, xproto::EventMask::NO_EVENT, notify));
                ok(conn.flush());
            }
        }
    })
}

#[test]
fn a_copy_still_being_captured_is_kept_and_the_newer_wins_on_both() {
    let rig = Rig::linked();
    // b's owner changes first, and answers slowly; a copies once b is capturing.
    let stop = Arc::new(AtomicBool::new(false));
    let (asked, capturing) = mpsc::channel();
    let slow = Duration::from_millis(800);
    let owner = slow_owner(&rig.b.x, "slow on b", slow, Arc::clone(&stop), asked);
    ok(capturing.recv_timeout(Duration::from_secs(10)));
    let on_a = copy(&rig.a.x, "newer on a");
    let mut last = (None, None);
    let until = Instant::now() + Duration::from_secs(10);
    while last != (Some("newer on a".to_owned()), Some("newer on a".to_owned())) {
        assert!(Instant::now() < until, "a's copy not on both: {last:?}");
        last = (paste(&rig.a.x), paste(&rig.b.x));
    }
    // b's own copy was not abandoned: captured, and history on both, behind a's.
    eventually("b's copy in both histories", || {
        previews(&rig.a.home) == ["newer on a", "slow on b"]
            && previews(&rig.b.home) == ["newer on a", "slow on b"]
    });
    stop.store(true, Ordering::Relaxed);
    let _ = owner.join();
    kill(on_a);
}

#[test]
fn copies_on_both_sides_of_a_lost_link_end_with_the_later() {
    let rig = Rig::linked();
    rig.relay.set(Faults { blackhole: true, ..Faults::default() });
    let earlier = copy(&rig.a.x, "earlier on a");
    eventually("a's copy", || {
        previews(&rig.a.home).first().map(String::as_str) == Some("earlier on a")
    });
    std::thread::sleep(Duration::from_millis(300));
    let later = copy(&rig.b.x, "later on b");
    eventually("b's copy", || {
        previews(&rig.b.home).first().map(String::as_str) == Some("later on b")
    });
    rig.relay.set(Faults::default());
    eventually("the later copy on both", || {
        paste(&rig.a.x).as_deref() == Some("later on b")
            && paste(&rig.b.x).as_deref() == Some("later on b")
    });
    kill(earlier);
    kill(later);
}

#[test]
fn a_delete_deletes_everywhere_pinned_or_away() {
    let mut rig = Rig::linked();
    let one = copy(&rig.a.x, "kept and pinned on b");
    eventually("it on b", || previews(&rig.b.home).contains(&"kept and pinned on b".to_owned()));
    let on_b = entries(&rig.b.home).first().map(|e| e.id).unwrap_or_default();
    assert_eq!(ask(&rig.b.home, Request::Pin { id: on_b, pinned: true }), Response::Ok);
    let on_a = entries(&rig.a.home).first().map(|e| e.id).unwrap_or_default();
    assert_eq!(ask(&rig.a.home, Request::Delete { id: on_a }), Response::Ok);
    eventually("it gone from b", || previews(&rig.b.home).is_empty());
    assert_eq!(paste(&rig.b.x), None, "b's clipboard emptied with it");
    kill(one);
    // b away: the delete reaches it when it returns.
    let two = copy(&rig.a.x, "deleted while b is away");
    eventually("it on b", || {
        previews(&rig.b.home).first().is_some_and(|p| p == "deleted while b is away")
    });
    let three = copy(&rig.a.x, "newest");
    eventually("the newest on b", || previews(&rig.b.home).first().is_some_and(|p| p == "newest"));
    rig.b.stop();
    let id = entries(&rig.a.home).get(1).map(|e| e.id).unwrap_or_default();
    assert_eq!(ask(&rig.a.home, Request::Delete { id }), Response::Ok);
    rig.b.restart();
    eventually("the delete on b", || previews(&rig.b.home) == ["newest"]);
    kill(two);
    kill(three);
}

#[test]
fn a_tool_copying_our_clipboard_back_makes_no_loop() {
    let rig = Rig::linked();
    let on_a = copy(&rig.a.x, "copied on a");
    eventually("it on b", || paste(&rig.b.x).as_deref() == Some("copied on a"));
    // On b, a tool that takes the clipboard with whatever it holds, again and again (CopyQ's
    // own-the-clipboard mode, a VM's agent).
    let mut held: Option<Child> = None;
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        if let Some(text) = paste(&rig.b.x) {
            if let Some(c) = held.take() {
                kill(c);
            }
            held = Some(copy(&rig.b.x, &text));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if let Some(c) = held {
        kill(c);
    }
    assert_eq!(said(&rig.b, "announced"), 0, "b announced nothing");
    assert_eq!(previews(&rig.a.home), ["copied on a"]);
    assert_eq!(paste(&rig.a.x).as_deref(), Some("copied on a"));
    kill(on_a);
}

#[test]
fn a_password_manager_clearing_the_clipboard_clears_it_on_both() {
    let rig = Rig::linked();
    let secret = copy(&rig.a.x, "hunter2");
    eventually("it on b", || paste(&rig.b.x).as_deref() == Some("hunter2"));
    // It gives the clipboard up: no owner.
    let (conn, _) = ok(x11rb::connect(Some(&rig.a.x.display)));
    let clipboard = ok(ok(conn.intern_atom(false, b"CLIPBOARD")).reply()).atom;
    ok(conn.set_selection_owner(x11rb::NONE, clipboard, x11rb::CURRENT_TIME));
    ok(conn.sync());
    eventually("b's clipboard empty", || paste(&rig.b.x).is_none());
    // History keeps it, on both.
    assert_eq!(previews(&rig.b.home), ["hunter2"]);
    assert_eq!(previews(&rig.a.home), ["hunter2"]);
    kill(secret);
}
