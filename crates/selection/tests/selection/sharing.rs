//! Serving an entry from another machine whose large targets are still on their way
//! (docs/sharing.md, "Paste"): offered at once, a paste of one waits for it, within 3 s, and is
//! never answered with part of it.

use std::sync::Arc;

use crate::support::{
    Call, FIREFOX, FakeX, Notice, XTERM, engine, name, notified, request, some, written,
};
use jackstay_clip::{Content, Format, Kept, Remote, Target, TargetData};
use jackstay_selection::{Event, Millis, Report, Timestamp};

const PNG: &[u8] = b"\x89PNG not really";

fn text() -> Target {
    let kept = some(Kept::new(name("UTF8_STRING"), Format::Bits8, b"hi".to_vec()).ok());
    Target { name: name("UTF8_STRING"), data: TargetData::Kept(kept) }
}

fn image(arrived: bool) -> Target {
    let data = if arrived {
        TargetData::Kept(some(Kept::new(name("image/png"), Format::Bits8, PNG.to_vec()).ok()))
    } else {
        TargetData::Remote(Remote {
            type_name: name("image/png"),
            format: Format::Bits8,
            size: PNG.len() as u64,
            sha256: [0; 32],
        })
    };
    Target { name: name("image/png"), data }
}

fn content(targets: Vec<Target>) -> Arc<Content> {
    Arc::new(some(Content::new(targets).ok()))
}

/// The engine serving text and an image still to come.
fn serving() -> (FakeX, jackstay_selection::Engine) {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.time = 2_000;
    assert_eq!(
        e.serve(&mut x, content(vec![text(), image(false)]), Millis(0)),
        vec![Report::Claimed]
    );
    x.take_calls();
    (x, e)
}

fn wanted_png() -> Vec<Report> {
    vec![Report::Wanted { target: name("image/png") }]
}

#[test]
fn a_target_still_to_come_is_offered_and_its_paste_waits_for_it() {
    let (mut x, mut e) = serving();
    send!(e, &mut x, request(&mut x, XTERM, "TARGETS", "T", 0), Millis(1));
    let (_, atoms) = some(written(&mut x, XTERM, "T"));
    let png = x.a("image/png").0;
    assert!(atoms.as_chunks::<4>().0.iter().any(|a| u32::from_ne_bytes(*a) == png));
    // The text is there: answered at once. The image is not: no answer yet, and the daemon is
    // told to fetch it.
    send!(e, &mut x, request(&mut x, XTERM, "UTF8_STRING", "P", 0), Millis(2));
    assert_eq!(notified(&mut x), Some(Notice::Sent(x.a("P"))));
    let reports = send!(e, &mut x, request(&mut x, FIREFOX, "image/png", "Q", 0), Millis(3));
    assert_eq!(reports, wanted_png());
    let calls = x.take_calls();
    assert!(!calls.iter().any(|c| matches!(c, Call::Notify { .. })), "{calls:?}");
    assert!(calls.contains(&Call::Watch { window: FIREFOX, on: true }));
    assert_eq!(e.next_deadline(), Some(Millis(3_003)));
    // It arrives: served in place, no new claim, and the paste answered whole.
    e.update(&mut x, content(vec![text(), image(true)]), Millis(500));
    let calls = x.take_calls();
    assert!(!calls.iter().any(|c| matches!(c, Call::SetOwner { .. })), "{calls:?}");
    assert!(calls.contains(&Call::Notify {
        requestor: FIREFOX,
        selection: x.a("CLIPBOARD"),
        target: x.a("image/png"),
        property: Some(x.a("Q")),
        time: Timestamp(0),
    }));
    assert!(calls.contains(&Call::Watch { window: FIREFOX, on: false }));
    assert_eq!(written(&mut x, FIREFOX, "Q").map(|w| w.1), Some(PNG.to_vec()));
    assert_eq!(e.next_deadline(), None);
}

#[test]
fn a_paste_that_waits_too_long_is_refused_before_any_byte() {
    let (mut x, mut e) = serving();
    send!(e, &mut x, request(&mut x, FIREFOX, "image/png", "Q", 0), Millis(1));
    assert_eq!(e.tick(&mut x, Millis(3_000)), Vec::<Report>::new());
    assert_eq!(notified(&mut x), None);
    e.tick(&mut x, Millis(3_001));
    assert_eq!(notified(&mut x), Some(Notice::Refused));
    assert_eq!(written(&mut x, FIREFOX, "Q"), None);
    // What arrives after is served to the next paste, not to the refused one.
    e.update(&mut x, content(vec![text(), image(true)]), Millis(3_002));
    assert_eq!(notified(&mut x), None);
}

#[test]
fn a_paste_waiting_when_the_clipboard_moves_on_is_refused() {
    let (mut x, mut e) = serving();
    send!(e, &mut x, request(&mut x, FIREFOX, "image/png", "Q", 0), Millis(1));
    // Another entry served, a newer claim, with an image of its own on the way: the paste asked
    // for the first one's, and is refused now, not answered later with the second's.
    x.time = 3_000;
    e.serve(&mut x, content(vec![text(), image(false)]), Millis(2));
    assert_eq!(notified(&mut x), Some(Notice::Refused));
    // And another client taking the clipboard.
    let (mut x, mut e) = serving();
    send!(e, &mut x, request(&mut x, FIREFOX, "image/png", "Q", 0), Millis(1));
    let clipboard = x.a("CLIPBOARD");
    send!(
        e,
        &mut x,
        Event::SelectionClear { selection: clipboard, time: Timestamp(2_500) },
        Millis(2)
    );
    assert_eq!(notified(&mut x), Some(Notice::Refused));
}

#[test]
fn a_multiple_waits_for_every_target_still_to_come() {
    let (mut x, mut e) = serving();
    let pairs: Vec<u8> = [("UTF8_STRING", "A"), ("image/png", "B")]
        .iter()
        .flat_map(|(t, p)| [x.a(t).0.to_ne_bytes(), x.a(p).0.to_ne_bytes()].concat())
        .collect();
    let list = x.a("LIST");
    x.put(XTERM, list, "ATOM_PAIR", Format::Bits32, &pairs);
    let reports = send!(e, &mut x, request(&mut x, XTERM, "MULTIPLE", "LIST", 0), Millis(1));
    assert_eq!(reports, wanted_png());
    assert_eq!(notified(&mut x), None);
    assert_eq!(written(&mut x, XTERM, "A"), None);
    e.update(&mut x, content(vec![text(), image(true)]), Millis(2));
    assert_eq!(written(&mut x, XTERM, "A").map(|w| w.1), Some(b"hi".to_vec()));
    assert_eq!(written(&mut x, XTERM, "B").map(|w| w.1), Some(PNG.to_vec()));
    assert_eq!(notified(&mut x), Some(Notice::Sent(list)));
}

#[test]
fn a_requestor_gone_while_it_waits_is_forgotten() {
    let (mut x, mut e) = serving();
    send!(e, &mut x, request(&mut x, FIREFOX, "image/png", "Q", 0), Millis(1));
    send!(e, &mut x, Event::DestroyNotify { window: FIREFOX }, Millis(2));
    e.update(&mut x, content(vec![text(), image(true)]), Millis(3));
    assert_eq!(notified(&mut x), None);
    assert_eq!(e.next_deadline(), None);
}

#[test]
fn an_update_while_not_serving_content_changes_nothing() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    e.update(&mut x, content(vec![text()]), Millis(1));
    assert!(!e.owns_clipboard());
    assert_eq!(x.take_calls(), Vec::<Call>::new());
}
