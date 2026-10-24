//! Event orders where what the server says now and what the queued events say disagree, and
//! transitions that must leave nothing pending (docs/selection.md, "Liveness" and "Keep-alive").

use jackstay_clip::Format;
use jackstay_selection::{Event, Millis, Report, Timestamp, Window};

use crate::support::{
    Call, FIREFOX, FakeX, Notice, OURS, QT_APP, XTERM, answer, captured, engine, firefox_copies,
    notified, one, owning, refuse, refuse_at, request, set, some, written,
};

fn deliver(
    x: &mut FakeX,
    e: &mut jackstay_selection::Engine,
    events: Vec<Event>,
    now: u64,
) -> Vec<Report> {
    events.into_iter().flat_map(|ev| send!(e, x, ev, Millis(now))).collect()
}

#[test]
fn a_late_refusal_from_an_older_conversion_is_not_the_new_owners() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    // Firefox claims at 10, and we ask for its TARGETS with time 10. Before that request reaches
    // the server, Qt claims at 20; Qt receives our time-10 request and refuses it, as Qt does for
    // a request older than its claim.
    send!(e, &mut x, set(&mut x, Some(FIREFOX), 10), Millis(0));
    x.take_calls();
    send!(e, &mut x, set(&mut x, Some(QT_APP), 20), Millis(1));
    let (_, qt_targets) = one(x.converts());
    let r = send!(e, &mut x, refuse_at(&mut x, "TARGETS", Timestamp(10)), Millis(2));
    assert_eq!(r, vec![]);
    assert_eq!(x.converts(), vec![], "no fall back to text-only for Qt");
    let list = x.atom_list(&["UTF8_STRING", "text/html"]);
    send!(
        e,
        &mut x,
        answer(&mut x, "TARGETS", qt_targets, "ATOM", Format::Bits32, &list),
        Millis(3)
    );
    assert_eq!(one(x.converts()).0, "UTF8_STRING");
}

#[test]
fn a_late_refusal_does_not_fail_the_target_being_converted() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    send!(e, &mut x, set(&mut x, Some(QT_APP), 20), Millis(0));
    let (_, tp) = one(x.converts());
    let list = x.atom_list(&["UTF8_STRING"]);
    send!(e, &mut x, answer(&mut x, "TARGETS", tp, "ATOM", Format::Bits32, &list), Millis(1));
    let (_, p) = one(x.converts());
    assert_eq!(
        send!(e, &mut x, refuse_at(&mut x, "UTF8_STRING", Timestamp(10)), Millis(2)),
        vec![]
    );
    let r = send!(
        e,
        &mut x,
        answer(&mut x, "UTF8_STRING", p, "UTF8_STRING", Format::Bits8, b"qt"),
        Millis(3)
    );
    assert!(some(captured(&r)).0.kept("UTF8_STRING").is_some());
}

#[test]
fn a_queued_claim_older_than_our_reselect_does_not_take_the_clipboard_back() {
    let mut x = FakeX::new();
    let mut e = owning(&mut x, "re-selected");
    // Firefox copied just before the user re-selected; its notification arrives after our claim.
    let stale = Event::OwnerChanged {
        selection: x.a("CLIPBOARD"),
        owner: Some(FIREFOX),
        change: jackstay_selection::OwnerChange::Set,
        timestamp: Timestamp(1_500),
        selection_timestamp: Timestamp(1_500),
    };
    assert_eq!(send!(e, &mut x, stale, Millis(1)), vec![]);
    let clear = Event::SelectionClear { selection: x.a("CLIPBOARD"), time: Timestamp(1_500) };
    assert_eq!(send!(e, &mut x, clear, Millis(2)), vec![]);
    assert!(e.owns_clipboard());
    send!(e, &mut x, request(&mut x, XTERM, "UTF8_STRING", "P", 0), Millis(3));
    assert_eq!(written(&mut x, XTERM, "P").map(|w| w.1), Some(b"re-selected".to_vec()));
}

#[test]
fn a_queued_death_of_the_previous_owner_does_not_unown_a_reselect() {
    let mut x = FakeX::new();
    let mut e = firefox_copies(&mut x, "firefox's", 300);
    // The user re-selects a history entry; Firefox's death notification is still queued.
    x.time = 2_000;
    e.serve(&mut x, crate::support::text_content("history"), Millis(1));
    let death = crate::support::died(&mut x, 300, 1_999);
    assert_eq!(send!(e, &mut x, death, Millis(2)), vec![]);
    assert!(e.owns_clipboard());
}

#[test]
fn a_new_claim_after_ours_is_honoured() {
    let mut x = FakeX::new();
    let mut e = owning(&mut x, "ours");
    x.time = 3_000;
    let evs = x_claims(&mut x, FIREFOX, 2_500);
    let r = deliver(&mut x, &mut e, evs, 1);
    assert_eq!(r, vec![Report::Lost, Report::CaptureStarted { capture: 1 }]);
    assert!(!e.owns_clipboard());
    assert_eq!(one(x.converts()).0, "TARGETS");
}

fn x_claims(x: &mut FakeX, owner: Window, time: u32) -> Vec<Event> {
    x.other_claims(owner, time)
}

#[test]
fn an_outstanding_probe_is_released_on_every_ownership_change() {
    let mut x = FakeX::new();
    let mut e = firefox_copies(&mut x, "hi", 300);
    e.tick(&mut x, Millis(30_000));
    assert_eq!(one(x.converts()).0, "TARGETS");
    assert_eq!(e.properties_in_use(), 1, "the probe");
    x.time = 5_000;
    let evs = x_claims(&mut x, QT_APP, 4_000);
    deliver(&mut x, &mut e, evs, 30_001);
    assert_eq!(e.properties_in_use(), 1, "only the new capture's TARGETS");
    e.clear(&mut x, Millis(30_002));
    assert_eq!(e.properties_in_use(), 0);
}

#[test]
fn save_targets_before_targets_is_answered_still_saves_everything() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    send!(e, &mut x, set(&mut x, Some(QT_APP), 300), Millis(0));
    let (_, tp) = one(x.converts());
    // Qt quits at once: SAVE_TARGETS with no list, before its TARGETS reply has arrived.
    let save = Event::SelectionRequest {
        requestor: QT_APP,
        selection: x.a("CLIPBOARD_MANAGER"),
        target: x.a("SAVE_TARGETS"),
        property: None,
        time: Timestamp(301),
    };
    assert_eq!(send!(e, &mut x, save, Millis(1)), vec![]);
    let list = x.atom_list(&["UTF8_STRING", "application/x-qt-image"]);
    send!(e, &mut x, answer(&mut x, "TARGETS", tp, "ATOM", Format::Bits32, &list), Millis(2));
    let (first, p) = one(x.converts());
    assert_eq!(first, "UTF8_STRING");
    send!(
        e,
        &mut x,
        answer(&mut x, "UTF8_STRING", p, "UTF8_STRING", Format::Bits8, b"qt"),
        Millis(3)
    );
    let (second, p) = one(x.converts());
    assert_eq!(second, "application/x-qt-image", "the save asked for everything");
    let r = send!(
        e,
        &mut x,
        answer(
            &mut x,
            "application/x-qt-image",
            p,
            "application/x-qt-image",
            Format::Bits8,
            b"img"
        ),
        Millis(4)
    );
    assert!(r.contains(&Report::SaveAnswered { taken: true }));
}

fn pending_save(x: &mut FakeX, e: &mut jackstay_selection::Engine) {
    let list = x.atom_list(&["application/x-slow"]);
    let sl = x.a("SL");
    x.put(FIREFOX, sl, "ATOM", Format::Bits32, &list);
    let save = Event::SelectionRequest {
        requestor: FIREFOX,
        selection: x.a("CLIPBOARD_MANAGER"),
        target: x.a("SAVE_TARGETS"),
        property: Some(sl),
        time: Timestamp(500),
    };
    send!(e, x, save, Millis(10));
    x.take_calls();
}

#[test]
fn a_pending_save_is_answered_when_the_clipboard_moves_on() {
    let mut x = FakeX::new();
    let mut e = firefox_copies(&mut x, "kept", 300);
    pending_save(&mut x, &mut e);
    x.time = 5_000;
    let evs = x_claims(&mut x, QT_APP, 4_000);
    let r = deliver(&mut x, &mut e, evs, 11);
    assert!(r.contains(&Report::SaveAnswered { taken: false }));
    assert!(x.calls.iter().any(
        |c| matches!(c, Call::Notify { requestor, property: None, .. } if *requestor == FIREFOX)
    ));

    let mut x = FakeX::new();
    let mut e = firefox_copies(&mut x, "kept", 300);
    pending_save(&mut x, &mut e);
    let r = e.clear(&mut x, Millis(11));
    assert!(r.contains(&Report::SaveAnswered { taken: false }));
}

#[test]
fn a_pending_save_ends_with_the_requestors_death_and_is_kept_alive() {
    let mut x = FakeX::new();
    let mut e = firefox_copies(&mut x, "kept", 300);
    pending_save(&mut x, &mut e);
    let evs = x.owner_dies();
    let r = deliver(&mut x, &mut e, evs, 11);
    assert!(r.contains(&Report::KeptAlive));
    assert!(r.contains(&Report::SaveAnswered { taken: true }));
    assert!(e.owns_clipboard());
}

#[test]
fn an_incr_targets_reply_quarantines_its_property() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    send!(e, &mut x, set(&mut x, Some(FIREFOX), 10), Millis(0));
    let (_, tp) = one(x.converts());
    send!(
        e,
        &mut x,
        answer(&mut x, "TARGETS", tp, "INCR", Format::Bits32, &100u32.to_ne_bytes()),
        Millis(1)
    );
    let mut later = Vec::new();
    for i in 0..12u32 {
        send!(e, &mut x, set(&mut x, Some(Window(900 + i)), 20 + i), Millis(2 + u64::from(i)));
        later.extend(x.converts().into_iter().map(|c| c.1));
    }
    assert!(!later.contains(&tp), "the owner may still be sending into it");
}

#[test]
fn the_incr_threshold_follows_the_servers_request_limit() {
    let mut x = FakeX::new();
    x.max_property = 1_000;
    let mut e = owning(&mut x, &"z".repeat(1_001));
    send!(e, &mut x, request(&mut x, XTERM, "UTF8_STRING", "P", 0), Millis(1));
    assert_eq!(written(&mut x, XTERM, "P").map(|w| w.0), Some("INCR".into()));
}

#[test]
fn a_small_reply_replacing_a_transfer_unwatches_its_requestor() {
    let mut x = FakeX::new();
    let mut e = owning(&mut x, &"z".repeat(600 * 1024));
    send!(e, &mut x, request(&mut x, XTERM, "UTF8_STRING", "P", 0), Millis(1));
    x.take_calls();
    send!(e, &mut x, request(&mut x, XTERM, "TIMESTAMP", "P", 0), Millis(2));
    assert!(x.take_calls().contains(&Call::Watch { window: XTERM, on: false }));
}

#[test]
fn a_cleared_clipboard_refuses_multiple() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    e.clear(&mut x, Millis(0));
    let list = x.a("LIST");
    let pairs: Vec<u8> = [x.a("TIMESTAMP").0.to_ne_bytes(), x.a("A").0.to_ne_bytes()].concat();
    x.put(XTERM, list, "ATOM_PAIR", Format::Bits32, &pairs);
    x.take_calls();
    send!(e, &mut x, request(&mut x, XTERM, "MULTIPLE", "LIST", 0), Millis(1));
    assert_eq!(notified(&mut x), Some(Notice::Refused));
}

#[test]
fn a_capture_cut_short_after_reporting_content_says_so() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    send!(e, &mut x, set(&mut x, Some(FIREFOX), 10), Millis(0));
    let (_, tp) = one(x.converts());
    let list = x.atom_list(&["UTF8_STRING", "image/png"]);
    send!(e, &mut x, answer(&mut x, "TARGETS", tp, "ATOM", Format::Bits32, &list), Millis(1));
    let (_, p) = one(x.converts());
    let r = send!(
        e,
        &mut x,
        answer(&mut x, "UTF8_STRING", p, "UTF8_STRING", Format::Bits8, b"t"),
        Millis(2)
    );
    assert!(!some(captured(&r)).1, "incomplete: the image is still coming");
    let r = send!(e, &mut x, set(&mut x, Some(QT_APP), 20), Millis(3));
    assert_eq!(r, vec![Report::CaptureEnded { capture: 1 }, Report::CaptureStarted { capture: 2 }]);
    // A capture that never reported anything ends silently.
    let r = send!(e, &mut x, set(&mut x, Some(FIREFOX), 30), Millis(4));
    assert_eq!(r, vec![Report::CaptureStarted { capture: 3 }]);
    let _ = (refuse, OURS);
}

#[test]
fn a_late_write_to_an_abandoned_property_is_deleted_at_once() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    send!(e, &mut x, set(&mut x, Some(FIREFOX), 10), Millis(0));
    let (_, abandoned) = one(x.converts());
    send!(e, &mut x, set(&mut x, Some(QT_APP), 20), Millis(1));
    x.take_calls();
    // Firefox answers the conversion we gave up on.
    x.put(OURS, abandoned, "ATOM", Format::Bits32, &[0; 4096]);
    let late = Event::PropertyNotify {
        window: OURS,
        atom: abandoned,
        state: jackstay_selection::PropertyState::NewValue,
        time: Timestamp(0),
    };
    assert_eq!(send!(e, &mut x, late, Millis(2)), vec![]);
    assert!(x.take_calls().contains(&Call::Delete { window: OURS, property: abandoned }));
    assert!(!x.props.contains_key(&(OURS, abandoned)));
}
