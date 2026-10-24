//! Boundaries, routing and bookkeeping: each test pins one decision the scenarios do not.

use jackstay_clip::{Format, NotKept, TargetData};
use jackstay_selection::{Event, Millis, OwnerChange, PropertyState, Report, Timestamp, Window};

use crate::support::{
    Call, FIREFOX, FakeX, Notice, OURS, XTERM, answer, captured, deleted, engine, firefox_copies,
    kept_text, new_value, notified, one, owning, refuse, request, set, some, written,
};

/// Starts a capture of `owner` offering `targets`; returns the property of the first conversion.
fn offer(
    x: &mut FakeX,
    e: &mut jackstay_selection::Engine,
    owner: Window,
    time: u32,
    targets: &[&str],
    now: u64,
) -> jackstay_selection::Atom {
    send!(e, x, set(x, Some(owner), time), Millis(now));
    let (_, tp) = one(x.converts());
    let list = x.atom_list(targets);
    send!(e, x, answer(x, "TARGETS", tp, "ATOM", Format::Bits32, &list), Millis(now));
    one(x.converts()).1
}

#[test]
fn a_quarantined_property_is_not_reused_until_its_quarantine_ends() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    // Ten owners in a row, each abandoned mid-TARGETS by the next.
    let mut abandoned = Vec::new();
    for i in 0..10u32 {
        send!(e, &mut x, set(&mut x, Some(Window(900 + i)), 10 + i), Millis(u64::from(i)));
        abandoned.push(one(x.converts()).1);
    }
    let unique: std::collections::HashSet<_> = abandoned.iter().collect();
    assert_eq!(unique.len(), 10, "none reused while quarantined");
    // Each was abandoned when the next owner arrived (at 1 ms, 2 ms, ...). Once the first one's
    // quarantine has ended, it is handed out again.
    send!(e, &mut x, set(&mut x, Some(Window(950)), 50), Millis(60_001));
    let (_, reused) = one(x.converts());
    assert_eq!(Some(&reused), abandoned.first(), "quarantine ends after 60 s");
}

#[test]
fn a_cleanly_finished_property_is_released_for_reuse() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    let mut used = Vec::new();
    for i in 0..5u32 {
        send!(e, &mut x, set(&mut x, Some(Window(900 + i)), 10 + i), Millis(u64::from(i)));
        let (_, tp) = one(x.converts());
        let list = x.atom_list(&["UTF8_STRING"]);
        send!(
            e,
            &mut x,
            answer(&mut x, "TARGETS", tp, "ATOM", Format::Bits32, &list),
            Millis(u64::from(i))
        );
        let (_, p) = one(x.converts());
        send!(
            e,
            &mut x,
            answer(&mut x, "UTF8_STRING", p, "UTF8_STRING", Format::Bits8, b"a"),
            Millis(u64::from(i))
        );
        used.extend([tp, p]);
    }
    // Ten clean conversions, and the pool interns eight before reusing any.
    let unique: std::collections::HashSet<_> = used.iter().collect();
    assert_eq!(unique.len(), 8);
}

#[test]
fn a_reply_for_another_target_is_not_taken_for_the_one_being_converted() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    let p = offer(&mut x, &mut e, FIREFOX, 10, &["UTF8_STRING", "text/html"], 0);
    let r = send!(
        e,
        &mut x,
        answer(&mut x, "text/html", p, "text/html", Format::Bits8, b"<p>"),
        Millis(1)
    );
    assert_eq!(r, vec![]);
    assert_eq!(x.converts(), vec![]);
    let r = send!(
        e,
        &mut x,
        answer(&mut x, "UTF8_STRING", p, "UTF8_STRING", Format::Bits8, b"t"),
        Millis(2)
    );
    assert_eq!(kept_text(&some(captured(&r)).0, "UTF8_STRING").as_deref(), Some("t"));
}

#[test]
fn primary_events_are_neither_captured_nor_served() {
    let mut x = FakeX::new();
    let mut e = owning(&mut x, "hi");
    let primary = x.a("PRIMARY");
    // Later than our claim, so only its selection keeps it from being taken as a new owner.
    let ev = Event::OwnerChanged {
        selection: primary,
        owner: Some(FIREFOX),
        change: OwnerChange::Set,
        timestamp: Timestamp(9_000),
        selection_timestamp: Timestamp(9_000),
    };
    send!(e, &mut x, ev, Millis(1));
    assert_eq!(x.converts(), vec![]);
    let ev = Event::SelectionNotify {
        selection: primary,
        target: x.a("TARGETS"),
        property: None,
        time: Timestamp(0),
    };
    assert_eq!(send!(e, &mut x, ev, Millis(2)), vec![]);
    let ev = Event::SelectionRequest {
        requestor: XTERM,
        selection: primary,
        target: x.a("UTF8_STRING"),
        property: Some(x.a("P")),
        time: Timestamp(0),
    };
    send!(e, &mut x, ev, Millis(3));
    assert_eq!(notified(&mut x), Some(Notice::Refused));
}

#[test]
fn selection_clear_reports_only_what_we_actually_owned() {
    let mut x = FakeX::new();
    let mut e = firefox_copies(&mut x, "theirs", 300);
    let clear =
        |x: &mut FakeX, s: &str| Event::SelectionClear { selection: x.a(s), time: Timestamp(0) };
    assert_eq!(
        send!(e, &mut x, clear(&mut x, "CLIPBOARD"), Millis(1)),
        vec![],
        "CLIPBOARD was not ours"
    );
    assert_eq!(
        send!(e, &mut x, clear(&mut x, "CLIPBOARD_MANAGER"), Millis(2)),
        vec![Report::ManagerLost]
    );
    assert_eq!(
        send!(e, &mut x, clear(&mut x, "CLIPBOARD_MANAGER"), Millis(3)),
        vec![],
        "already lost"
    );
}

#[test]
fn property_notifications_are_routed_by_window_and_state() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    let p = offer(&mut x, &mut e, FIREFOX, 10, &["UTF8_STRING"], 0);
    send!(
        e,
        &mut x,
        answer(&mut x, "UTF8_STRING", p, "INCR", Format::Bits32, &4u32.to_ne_bytes()),
        Millis(1)
    );
    // A NewValue on another client's window, or a Delete on ours, is not an INCR chunk.
    x.put(OURS, p, "UTF8_STRING", Format::Bits8, b"abcd");
    let other = Event::PropertyNotify {
        window: XTERM,
        atom: p,
        state: PropertyState::NewValue,
        time: Timestamp(0),
    };
    assert_eq!(send!(e, &mut x, other, Millis(2)), vec![]);
    assert_eq!(send!(e, &mut x, deleted(OURS, p), Millis(3)), vec![]);
    assert!(x.props.contains_key(&(OURS, p)), "the chunk is still unread");
    send!(e, &mut x, new_value(p), Millis(4));
    assert!(!x.props.contains_key(&(OURS, p)));
}

#[test]
fn the_manager_answers_targets_refuses_other_targets_and_one_save_at_a_time() {
    let mut x = FakeX::new();
    let mut e = firefox_copies(&mut x, "kept", 300);
    let manager_req = |x: &mut FakeX, target: &str| Event::SelectionRequest {
        requestor: FIREFOX,
        selection: x.a("CLIPBOARD_MANAGER"),
        target: x.a(target),
        property: Some(x.a("MP")),
        time: Timestamp(0),
    };
    send!(e, &mut x, manager_req(&mut x, "TARGETS"), Millis(1));
    let (_, data) = some(written(&mut x, FIREFOX, "MP"));
    let names: Vec<String> = data
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| x.name_of(jackstay_selection::Atom(u32::from_ne_bytes(*b))))
        .collect();
    assert_eq!(names, ["TARGETS", "SAVE_TARGETS", "TIMESTAMP"]);
    send!(e, &mut x, manager_req(&mut x, "UTF8_STRING"), Millis(2));
    assert_eq!(notified(&mut x), Some(Notice::Refused));
    // A first SAVE_TARGETS waits on a slow target; a second one meanwhile is refused.
    let list = x.atom_list(&["application/x-slow"]);
    let mp = x.a("MP");
    x.put(FIREFOX, mp, "ATOM", Format::Bits32, &list);
    send!(e, &mut x, manager_req(&mut x, "SAVE_TARGETS"), Millis(3));
    x.take_calls();
    send!(e, &mut x, manager_req(&mut x, "SAVE_TARGETS"), Millis(4));
    assert_eq!(notified(&mut x), Some(Notice::Refused));
}

#[test]
fn save_targets_from_an_owner_we_never_captured_takes_nothing() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.claim("CLIPBOARD", FIREFOX, 10);
    let ev = Event::SelectionRequest {
        requestor: FIREFOX,
        selection: x.a("CLIPBOARD_MANAGER"),
        target: x.a("SAVE_TARGETS"),
        property: None,
        time: Timestamp(0),
    };
    assert_eq!(send!(e, &mut x, ev, Millis(1)), vec![Report::SaveAnswered { taken: false }]);
}

#[test]
fn save_targets_converts_nothing_twice() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    let p = offer(&mut x, &mut e, FIREFOX, 10, &["UTF8_STRING", "text/html", "image/png"], 0);
    send!(
        e,
        &mut x,
        answer(&mut x, "UTF8_STRING", p, "UTF8_STRING", Format::Bits8, b"t"),
        Millis(1)
    );
    let (current, mut p) = one(x.converts());
    assert_eq!(current, "text/html");
    // The exiting app lists what is held, what is being converted, what is queued, and one more
    // target twice.
    let names =
        ["UTF8_STRING", "text/html", "image/png", "application/x-more", "application/x-more"];
    let list = x.atom_list(&names);
    let sl = x.a("SL");
    x.put(FIREFOX, sl, "ATOM", Format::Bits32, &list);
    let ev = Event::SelectionRequest {
        requestor: FIREFOX,
        selection: x.a("CLIPBOARD_MANAGER"),
        target: x.a("SAVE_TARGETS"),
        property: Some(sl),
        time: Timestamp(400),
    };
    send!(e, &mut x, ev, Millis(2));
    assert_eq!(x.converts(), vec![], "nothing new starts while text/html is outstanding");
    let mut converted = Vec::new();
    for (i, target) in ["text/html", "image/png", "application/x-more"].into_iter().enumerate() {
        let at = Millis(3 + u64::try_from(i).unwrap_or(0));
        send!(e, &mut x, answer(&mut x, target, p, target, Format::Bits8, b"x"), at);
        if let Some((next, next_p)) = x.converts().pop() {
            converted.push(next);
            p = next_p;
        }
    }
    assert_eq!(converted, ["image/png", "application/x-more"]);
}

#[test]
fn a_capture_that_runs_out_of_time_keeps_what_it_has_and_marks_the_rest_failed() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    let mut p = offer(
        &mut x,
        &mut e,
        FIREFOX,
        10,
        &["UTF8_STRING", "text/html", "text/rtf", "image/png"],
        0,
    );
    // The owner answers text at once, then trickles one INCR byte every 4.9 s.
    send!(
        e,
        &mut x,
        answer(&mut x, "UTF8_STRING", p, "UTF8_STRING", Format::Bits8, b"t"),
        Millis(1)
    );
    p = one(x.converts()).1;
    send!(
        e,
        &mut x,
        answer(&mut x, "text/html", p, "INCR", Format::Bits32, &100u32.to_ne_bytes()),
        Millis(2)
    );
    let mut now = 2;
    let mut last = Vec::new();
    while now < 61_000 {
        now += 4_900;
        x.put(OURS, p, "text/html", Format::Bits8, b"x");
        send!(e, &mut x, new_value(p), Millis(now));
        last = e.tick(&mut x, Millis(now));
        if !last.is_empty() {
            break;
        }
    }
    let (content, complete) = some(captured(&last));
    assert!(complete && now <= 60_000 + 4_900, "ended at {now}");
    assert_eq!(kept_text(&content, "UTF8_STRING").as_deref(), Some("t"));
    for t in ["text/html", "text/rtf", "image/png"] {
        let data = content.targets().iter().find(|x| x.name.as_str() == t).map(|x| x.data.clone());
        assert_eq!(data, Some(TargetData::NotKept(NotKept::OwnerFailed)), "{t}");
    }
}

#[test]
fn a_targets_reply_of_the_wrong_type_or_too_large_counts_as_refused() {
    for (type_, data) in [("INTEGER", vec![0u8; 8]), ("ATOM", vec![0u8; 64 * 1024 + 4])] {
        let mut x = FakeX::new();
        let (mut e, _) = engine(&mut x);
        x.take_calls();
        send!(e, &mut x, set(&mut x, Some(FIREFOX), 10), Millis(0));
        let (_, tp) = one(x.converts());
        send!(e, &mut x, answer(&mut x, "TARGETS", tp, type_, Format::Bits32, &data), Millis(1));
        assert_eq!(one(x.converts()).0, "UTF8_STRING", "fallbacks after a {type_} TARGETS");
    }
}

#[test]
fn targets_type_targets_is_accepted_and_none_and_repeats_are_dropped() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    send!(e, &mut x, set(&mut x, Some(FIREFOX), 10), Millis(0));
    let (_, tp) = one(x.converts());
    let mut list = x.atom_list(&["text/html", "text/html"]);
    list.extend(0u32.to_ne_bytes());
    send!(e, &mut x, answer(&mut x, "TARGETS", tp, "TARGETS", Format::Bits32, &list), Millis(1));
    let (target, p) = one(x.converts());
    assert_eq!(target, "text/html");
    let r = send!(
        e,
        &mut x,
        answer(&mut x, "text/html", p, "text/html", Format::Bits8, b"<p>"),
        Millis(2)
    );
    let (content, complete) = some(captured(&r));
    assert!(complete);
    assert_eq!(content.targets().len(), 1);
}

#[test]
fn without_png_the_first_raster_image_is_fetched_not_a_vector_one() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    let p = offer(&mut x, &mut e, FIREFOX, 10, &["image/svg+xml", "image/bmp", "image/jpeg"], 0);
    send!(
        e,
        &mut x,
        answer(&mut x, "image/svg+xml", p, "image/svg+xml", Format::Bits8, b"<svg/>"),
        Millis(1)
    );
    assert_eq!(one(x.converts()).0, "image/bmp");
}

#[test]
fn an_incr_announcing_exactly_the_room_left_is_received() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    let p = offer(&mut x, &mut e, FIREFOX, 10, &["UTF8_STRING"], 0);
    let room = u32::try_from(crate::support::limits().per_target).unwrap_or(0);
    send!(
        e,
        &mut x,
        answer(&mut x, "UTF8_STRING", p, "INCR", Format::Bits32, &room.to_ne_bytes()),
        Millis(1)
    );
    x.put(OURS, p, "UTF8_STRING", Format::Bits8, b"");
    let r = send!(e, &mut x, new_value(p), Millis(2));
    assert!(some(captured(&r)).0.kept("UTF8_STRING").is_some());
}

#[test]
fn an_empty_fallback_answer_does_not_end_the_fallbacks() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    send!(e, &mut x, set(&mut x, Some(XTERM), 10), Millis(0));
    x.take_calls();
    send!(e, &mut x, refuse(&mut x, "TARGETS"), Millis(1));
    let (_, p) = one(x.converts());
    send!(
        e,
        &mut x,
        answer(&mut x, "UTF8_STRING", p, "UTF8_STRING", Format::Bits8, b""),
        Millis(2)
    );
    assert_eq!(one(x.converts()).0, "STRING");
}

#[test]
fn a_capture_that_keeps_nothing_is_reported_failed() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    send!(e, &mut x, set(&mut x, Some(XTERM), 10), Millis(0));
    x.take_calls();
    let mut all = send!(e, &mut x, refuse(&mut x, "TARGETS"), Millis(1));
    for target in ["UTF8_STRING", "STRING", "image/png"] {
        all.extend(send!(e, &mut x, refuse(&mut x, target), Millis(2)));
    }
    assert_eq!(all, vec![Report::CaptureFailed { capture: 1 }], "reported once, at the end");
}

#[test]
fn no_probe_is_sent_while_a_capture_is_still_talking_to_the_owner() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    let p = offer(&mut x, &mut e, FIREFOX, 10, &["UTF8_STRING"], 0);
    // A slow INCR owner keeps the capture alive past the probe's 30 s.
    let size = 64u32.to_ne_bytes();
    send!(e, &mut x, answer(&mut x, "UTF8_STRING", p, "INCR", Format::Bits32, &size), Millis(1));
    let mut probes = 0;
    for now in (4_000..=40_000).step_by(4_000) {
        x.put(OURS, p, "UTF8_STRING", Format::Bits8, b"x");
        send!(e, &mut x, new_value(p), Millis(now));
        e.tick(&mut x, Millis(now));
        probes += x.converts().iter().filter(|c| c.0 == "TARGETS").count();
    }
    assert_eq!(probes, 0);
}

#[test]
fn a_probe_answer_is_read_and_deleted() {
    let mut x = FakeX::new();
    let mut e = firefox_copies(&mut x, "hi", 300);
    x.take_calls();
    e.tick(&mut x, Millis(30_000));
    let (target, p) = one(x.converts());
    assert_eq!(target, "TARGETS");
    let other = send!(e, &mut x, refuse(&mut x, "UTF8_STRING"), Millis(30_001));
    assert_eq!(other, vec![], "a reply for another target is not the probe's");
    let list = x.atom_list(&["UTF8_STRING"]);
    send!(e, &mut x, answer(&mut x, "TARGETS", p, "ATOM", Format::Bits32, &list), Millis(30_002));
    assert!(!x.props.contains_key(&(OURS, p)));
}

#[test]
fn the_incr_threshold_is_exactly_256_kib() {
    for (len, incr) in [(256 * 1024, false), (256 * 1024 + 1, true)] {
        let mut x = FakeX::new();
        let mut e = owning(&mut x, &"z".repeat(len));
        send!(e, &mut x, request(&mut x, XTERM, "UTF8_STRING", "P", 0), Millis(1));
        assert_eq!(some(written(&mut x, XTERM, "P")).0 == "INCR", incr, "{len} bytes");
    }
}

#[test]
fn text_plain_utf8_is_served() {
    let mut x = FakeX::new();
    let mut e = owning(&mut x, "→");
    send!(e, &mut x, request(&mut x, XTERM, "text/plain;charset=utf-8", "P", 0), Millis(1));
    assert_eq!(written(&mut x, XTERM, "P"), Some(("UTF8_STRING".into(), "→".as_bytes().to_vec())));
}

#[test]
fn a_malformed_multiple_list_is_refused() {
    let mut x = FakeX::new();
    let mut e = owning(&mut x, "hi");
    let list = x.a("LIST");
    x.put(XTERM, list, "ATOM_PAIR", Format::Bits32, &[0; 12]);
    send!(e, &mut x, request(&mut x, XTERM, "MULTIPLE", "LIST", 0), Millis(1));
    assert_eq!(notified(&mut x), Some(Notice::Refused));
    x.put(XTERM, list, "ATOM_PAIR", Format::Bits8, &[0; 16]);
    send!(e, &mut x, request(&mut x, XTERM, "MULTIPLE", "LIST", 0), Millis(2));
    assert_eq!(notified(&mut x), Some(Notice::Refused));
}

#[test]
fn an_idle_transfer_lives_until_its_deadline() {
    let mut x = FakeX::new();
    let mut e = owning(&mut x, &"x".repeat(600 * 1024));
    send!(e, &mut x, request(&mut x, XTERM, "UTF8_STRING", "P", 0), Millis(1));
    e.tick(&mut x, Millis(5_000));
    assert_eq!(e.next_deadline(), Some(Millis(10_001)));
}

#[test]
fn a_requestor_stays_watched_while_another_transfer_to_it_is_live() {
    let mut x = FakeX::new();
    let mut e = owning(&mut x, &"x".repeat(600 * 1024));
    send!(e, &mut x, request(&mut x, XTERM, "UTF8_STRING", "P", 0), Millis(1));
    send!(e, &mut x, request(&mut x, XTERM, "UTF8_STRING", "Q", 0), Millis(2));
    let p = x.a("P");
    x.take_calls();
    for i in 0..5 {
        send!(e, &mut x, deleted(XTERM, p), Millis(3 + i));
    }
    assert!(!x.take_calls().contains(&Call::Watch { window: XTERM, on: false }));
}

#[test]
fn text_held_only_as_string_is_served_as_utf8_too() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    send!(e, &mut x, set(&mut x, Some(XTERM), 300), Millis(0));
    x.take_calls();
    send!(e, &mut x, refuse(&mut x, "TARGETS"), Millis(1));
    x.take_calls();
    send!(e, &mut x, refuse(&mut x, "UTF8_STRING"), Millis(2));
    let (_, p) = one(x.converts());
    send!(e, &mut x, answer(&mut x, "STRING", p, "STRING", Format::Bits8, b"caf\xe9"), Millis(3));
    x.time = 900;
    send!(e, &mut x, crate::support::died(&mut x, 300, 900), Millis(4));
    send!(e, &mut x, request(&mut x, FIREFOX, "UTF8_STRING", "P", 950), Millis(5));
    assert_eq!(
        written(&mut x, FIREFOX, "P"),
        Some(("UTF8_STRING".into(), "café".as_bytes().to_vec()))
    );
}

#[test]
fn half_the_timestamp_circle_is_the_boundary() {
    assert!(Timestamp(0).is_earlier_than(Timestamp(0x7FFF_FFFF)));
    assert!(!Timestamp(0).is_earlier_than(Timestamp(0x8000_0000)));
    assert_eq!(Millis(42).to_string(), "42 ms");
}

#[test]
fn a_large_but_valid_targets_list_is_read() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    send!(e, &mut x, set(&mut x, Some(FIREFOX), 10), Millis(0));
    let (_, tp) = one(x.converts());
    let mut names: Vec<String> = (0..2_000).map(|i| format!("application/x-many-{i}")).collect();
    // Not a fallback target, so only a list that was actually read can lead here.
    names.push("text/html".into());
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let list = x.atom_list(&refs);
    assert!(list.len() > 8_000 && list.len() < 64 * 1024);
    send!(e, &mut x, answer(&mut x, "TARGETS", tp, "ATOM", Format::Bits32, &list), Millis(1));
    assert_eq!(one(x.converts()).0, "text/html");
}

#[test]
fn a_primary_reply_is_not_taken_for_the_clipboard_capture() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    let p = offer(&mut x, &mut e, FIREFOX, 10, &["UTF8_STRING"], 0);
    x.put(OURS, p, "UTF8_STRING", Format::Bits8, b"from primary");
    let primary = Event::SelectionNotify {
        selection: x.a("PRIMARY"),
        target: x.a("UTF8_STRING"),
        property: Some(p),
        time: Timestamp(0),
    };
    assert_eq!(send!(e, &mut x, primary, Millis(1)), vec![]);
    assert!(x.props.contains_key(&(OURS, p)), "not read");
}

#[test]
fn a_reply_on_our_property_with_another_time_is_not_this_conversions() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    let p = offer(&mut x, &mut e, FIREFOX, 10, &["UTF8_STRING"], 0);
    // An owner of an earlier conversion that used this property, answering late.
    let late = crate::support::answer_at(
        &mut x,
        "UTF8_STRING",
        p,
        "UTF8_STRING",
        Format::Bits8,
        b"old",
        Timestamp(7),
    );
    assert_eq!(send!(e, &mut x, late, Millis(1)), vec![]);
    let current = crate::support::answer_at(
        &mut x,
        "UTF8_STRING",
        p,
        "UTF8_STRING",
        Format::Bits8,
        b"new",
        Timestamp(0),
    );
    let r = send!(e, &mut x, current, Millis(2));
    assert_eq!(kept_text(&some(captured(&r)).0, "UTF8_STRING").as_deref(), Some("new"));
}

#[test]
fn the_probe_takes_only_answers_that_match_its_time() {
    let mut x = FakeX::new();
    let mut e = firefox_copies(&mut x, "hi", 300);
    x.take_calls();
    e.tick(&mut x, Millis(30_000));
    let (_, p) = one(x.converts());
    // A refusal, and a reply on its property, both with another time: neither is the probe's.
    assert_eq!(
        send!(
            e,
            &mut x,
            crate::support::refuse_at(&mut x, "TARGETS", Timestamp(299)),
            Millis(30_001)
        ),
        vec![]
    );
    let list = x.atom_list(&["UTF8_STRING"]);
    let wrong = crate::support::answer_at(
        &mut x,
        "TARGETS",
        p,
        "ATOM",
        Format::Bits32,
        &list,
        Timestamp(299),
    );
    assert_eq!(send!(e, &mut x, wrong, Millis(30_002)), vec![]);
    assert!(x.props.contains_key(&(OURS, p)), "left unread");
    assert_eq!(e.properties_in_use(), 1, "still outstanding");
    assert_eq!(send!(e, &mut x, refuse(&mut x, "TARGETS"), Millis(30_003)), vec![]);
    assert_eq!(e.properties_in_use(), 0, "a refusal with its own time answers it");
}

#[test]
fn ticks_between_probe_deadlines_count_no_miss() {
    let mut x = FakeX::new();
    let mut e = firefox_copies(&mut x, "hi", 300);
    let mut reported_at = None;
    for t in [30_000, 35_000, 40_000, 55_000, 70_000, 75_000, 80_000] {
        if e.tick(&mut x, Millis(t)).contains(&Report::OwnerHung) && reported_at.is_none() {
            reported_at = Some(t);
        }
    }
    assert_eq!(reported_at, Some(80_000));
}
