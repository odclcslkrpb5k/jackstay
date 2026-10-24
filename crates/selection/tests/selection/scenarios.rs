//! Scripted protocol scenarios, one per behaviour in docs/selection.md.

use crate::support::{
    Call, FIREFOX, FakeX, Notice, OURS, QT_APP, XTERM, answer, captured, deleted, died, engine,
    firefox_copies, kept_text, new_value, notified, one, owning, refuse, request, set, some,
    written,
};
use jackstay_clip::{Format, NotKept, TargetData};
use jackstay_selection::{
    Atom, Event, Millis, OwnerChange, Report, Timestamp, Window, compound_text, latin1,
};

#[test]
fn start_claims_clipboard_manager_and_announces_it() {
    let mut x = FakeX::new();
    let (_, reports) = engine(&mut x);
    assert_eq!(reports, vec![]);
    let manager = x.a("CLIPBOARD_MANAGER");
    assert_eq!(x.owners.get(&manager).and_then(|o| o.0), Some(OURS));
    assert!(x.calls.contains(&Call::Announce));
}

#[test]
fn another_manager_is_reported_and_left_alone() {
    let mut x = FakeX::new();
    x.claim("CLIPBOARD_MANAGER", Window(42), 10);
    let (_, reports) = engine(&mut x);
    assert_eq!(reports, vec![Report::ManagerConflict]);
    assert!(!x.calls.contains(&Call::Announce));
}

#[test]
fn capture_converts_with_the_selection_time_and_fetches_text_first() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    send!(e, &mut x, set(&mut x, Some(FIREFOX), 777), Millis(0));
    let calls = x.take_calls();
    let clipboard = x.a("CLIPBOARD");
    let targets = x.a("TARGETS");
    assert!(matches!(
        calls.as_slice(),
        [Call::Convert { selection, target, time: Timestamp(777), .. }] if *selection == clipboard && *target == targets
    ));
    let Call::Convert { property: tp, .. } = calls[0] else { panic!() };
    let list = x.atom_list(&[
        "TARGETS",
        "image/png",
        "text/html",
        "DELETE",
        "UTF8_STRING",
        "application/x-foo",
        "STRING",
        "UTF8_STRING",
    ]);
    let r =
        send!(e, &mut x, answer(&mut x, "TARGETS", tp, "ATOM", Format::Bits32, &list), Millis(1));
    assert_eq!(r, vec![]);
    let (first, p1) = one(x.converts());
    assert_eq!(first, "UTF8_STRING");
    assert_ne!(p1, tp, "each conversion gets a fresh property");
    let r = send!(
        e,
        &mut x,
        answer(&mut x, "UTF8_STRING", p1, "UTF8_STRING", Format::Bits8, b"hi"),
        Millis(2),
    );
    let (partial, complete) = some(captured(&r));
    assert!(!complete);
    assert_eq!(kept_text(&partial, "UTF8_STRING").as_deref(), Some("hi"));
    let (second, p2) = one(x.converts());
    assert_eq!(second, "text/html");
    send!(
        e,
        &mut x,
        answer(&mut x, "text/html", p2, "text/html", Format::Bits8, b"<b>hi</b>"),
        Millis(3),
    );
    let (third, p3) = one(x.converts());
    assert_eq!(third, "image/png");
    let r = send!(
        e,
        &mut x,
        answer(&mut x, "image/png", p3, "image/png", Format::Bits8, b"\x89PNG"),
        Millis(4),
    );
    let (content, complete) = some(captured(&r));
    assert!(complete);
    let not_canonical = |n: &str| {
        content.targets().iter().find(|t| t.name.as_str() == n).map(|t| t.data.clone())
            == Some(TargetData::NotKept(NotKept::NotCanonical))
    };
    assert!(
        not_canonical("DELETE") && not_canonical("STRING") && not_canonical("application/x-foo")
    );
    assert_eq!(kept_text(&content, "text/html").as_deref(), Some("<b>hi</b>"));
    assert!(content.kept("image/png").is_some());
    assert_eq!(x.converts(), vec![], "DELETE and the rest are never converted");
}

#[test]
fn an_owner_without_targets_gets_the_fallbacks_in_order() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    send!(e, &mut x, set(&mut x, Some(XTERM), 50), Millis(0));
    x.take_calls();
    send!(e, &mut x, refuse(&mut x, "TARGETS"), Millis(1));
    assert_eq!(one(x.converts()).0, "UTF8_STRING");
    send!(e, &mut x, refuse(&mut x, "UTF8_STRING"), Millis(2));
    let (string, p) = one(x.converts());
    assert_eq!(string, "STRING");
    let r = send!(
        e,
        &mut x,
        answer(&mut x, "STRING", p, "STRING", Format::Bits8, b"caf\xe9"),
        Millis(3),
    );
    let (content, complete) = some(captured(&r));
    assert!(complete, "the first fallback kept ends the capture");
    assert_eq!(content.kept("STRING").map(|k| k.bytes().to_vec()), Some(b"caf\xe9".to_vec()));
    assert_eq!(x.converts(), vec![]);
}

#[test]
fn an_owner_that_never_answers_targets_times_out_into_the_fallbacks() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    send!(e, &mut x, set(&mut x, Some(XTERM), 50), Millis(0));
    x.take_calls();
    assert_eq!(e.next_deadline(), Some(Millis(10_000)));
    e.tick(&mut x, Millis(9_999));
    assert_eq!(x.converts(), vec![]);
    e.tick(&mut x, Millis(10_000));
    assert_eq!(one(x.converts()).0, "UTF8_STRING");
}

#[test]
fn incr_is_received_chunk_by_chunk() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    send!(e, &mut x, set(&mut x, Some(FIREFOX), 9), Millis(0));
    let (_, tp) = one(x.converts());
    let list = x.atom_list(&["UTF8_STRING"]);
    send!(e, &mut x, answer(&mut x, "TARGETS", tp, "ATOM", Format::Bits32, &list), Millis(1));
    let (_, p) = one(x.converts());
    send!(
        e,
        &mut x,
        answer(&mut x, "UTF8_STRING", p, "INCR", Format::Bits32, &10u32.to_ne_bytes()),
        Millis(2),
    );
    assert!(
        !x.props.contains_key(&(OURS, p)),
        "reading the INCR property deletes it, which starts the owner"
    );
    // A NewValue on some other property is not ours to read.
    assert_eq!(send!(e, &mut x, new_value(Atom(9999)), Millis(3)), vec![]);
    for (i, chunk) in [&b"hello "[..], b"world", b""].iter().enumerate() {
        x.put(OURS, p, "UTF8_STRING", Format::Bits8, chunk);
        let r = send!(e, &mut x, new_value(p), Millis(4 + i as u64));
        if chunk.is_empty() {
            let (content, complete) = some(captured(&r));
            assert!(complete);
            assert_eq!(kept_text(&content, "UTF8_STRING").as_deref(), Some("hello world"));
        } else {
            assert_eq!(r, vec![]);
            assert!(!x.props.contains_key(&(OURS, p)));
        }
    }
}

#[test]
fn an_incr_owner_announcing_too_much_is_abandoned_as_too_large() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    send!(e, &mut x, set(&mut x, Some(FIREFOX), 9), Millis(0));
    let (_, tp) = one(x.converts());
    let list = x.atom_list(&["UTF8_STRING", "image/png"]);
    send!(e, &mut x, answer(&mut x, "TARGETS", tp, "ATOM", Format::Bits32, &list), Millis(1));
    let (_, p) = one(x.converts());
    send!(
        e,
        &mut x,
        answer(&mut x, "UTF8_STRING", p, "UTF8_STRING", Format::Bits8, b"t"),
        Millis(2),
    );
    let (_, img) = one(x.converts());
    let huge = (64u32 << 20).to_ne_bytes();
    let r = send!(
        e,
        &mut x,
        answer(&mut x, "image/png", img, "INCR", Format::Bits32, &huge),
        Millis(3),
    );
    let (content, complete) = some(captured(&r));
    assert!(complete);
    let png =
        content.targets().iter().find(|t| t.name.as_str() == "image/png").map(|t| t.data.clone());
    assert_eq!(png, Some(TargetData::NotKept(NotKept::TooLarge)));
    // The abandoned property is quarantined: chunks the owner still sends are not read.
    x.put(OURS, img, "image/png", Format::Bits8, b"late");
    assert_eq!(send!(e, &mut x, new_value(img), Millis(4)), vec![]);
}

#[test]
fn a_late_reply_to_an_abandoned_capture_is_not_read_as_the_new_copy() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    send!(e, &mut x, set(&mut x, Some(FIREFOX), 10), Millis(0));
    let (_, old) = one(x.converts());
    send!(e, &mut x, set(&mut x, Some(QT_APP), 20), Millis(1));
    let (_, new) = one(x.converts());
    assert_ne!(old, new);
    // Firefox's late TARGETS reply arrives on the old property.
    let list = x.atom_list(&["UTF8_STRING"]);
    assert_eq!(
        send!(e, &mut x, answer(&mut x, "TARGETS", old, "ATOM", Format::Bits32, &list), Millis(2)),
        vec![]
    );
    assert_eq!(x.converts(), vec![]);
    // Within the quarantine, the old property is not handed out again.
    let list = x.atom_list(&["UTF8_STRING"]);
    send!(e, &mut x, answer(&mut x, "TARGETS", new, "ATOM", Format::Bits32, &list), Millis(3));
    let (_, next) = one(x.converts());
    assert_ne!(next, old);
}

#[test]
fn the_owner_dying_is_kept_alive_with_its_own_claim_time() {
    let mut x = FakeX::new();
    let mut e = firefox_copies(&mut x, "hello", 300);
    x.take_calls();
    x.time = 900;
    let r = send!(e, &mut x, died(&mut x, 300, 900), Millis(10));
    assert_eq!(r, vec![Report::KeptAlive]);
    let clipboard = x.a("CLIPBOARD");
    assert!(x.take_calls().contains(&Call::SetOwner {
        selection: clipboard,
        owner: Some(OURS),
        time: Timestamp(300)
    }));
    send!(e, &mut x, request(&mut x, XTERM, "UTF8_STRING", "XSEL", 950), Millis(11));
    let xsel = x.a("XSEL");
    let calls = x.take_calls();
    assert!(calls.iter().any(
        |c| matches!(c, Call::Change { window, data, .. } if *window == XTERM && data == b"hello")
    ));
    assert!(calls.contains(&Call::Notify {
        requestor: XTERM,
        selection: clipboard,
        target: x.a("UTF8_STRING"),
        property: Some(xsel),
        time: Timestamp(950)
    }));
}

#[test]
fn keep_alive_never_overwrites_a_client_that_claimed_after_the_death() {
    let mut x = FakeX::new();
    let mut e = firefox_copies(&mut x, "old", 300);
    // Firefox dies at 900; xterm claims at 850 (an older user-event time, but after Firefox's 300)
    // before our event is handled.
    x.time = 900;
    let death = died(&mut x, 300, 900);
    x.claim("CLIPBOARD", XTERM, 850);
    let r = send!(e, &mut x, death, Millis(10));
    assert_eq!(r, vec![]);
    let clipboard = x.a("CLIPBOARD");
    assert_eq!(x.owners.get(&clipboard).and_then(|o| o.0), Some(XTERM));
}

#[test]
fn a_new_owner_ends_the_candidate_so_copy_and_quit_never_resurrects_the_previous_copy() {
    let mut x = FakeX::new();
    let mut e = firefox_copies(&mut x, "previous", 300);
    // A copy-and-quit tool claims at 400 and exits before anything is captured.
    send!(e, &mut x, set(&mut x, Some(Window(800)), 400), Millis(10));
    x.take_calls();
    let r = send!(e, &mut x, died(&mut x, 400, 401), Millis(11));
    assert_eq!(r, vec![]);
    assert!(!x.take_calls().iter().any(|c| matches!(c, Call::SetOwner { .. })));
}

#[test]
fn a_death_with_another_selection_time_is_not_ours_to_keep() {
    let mut x = FakeX::new();
    let mut e = firefox_copies(&mut x, "hi", 300);
    x.take_calls();
    assert_eq!(send!(e, &mut x, died(&mut x, 299, 900), Millis(10)), vec![]);
    assert!(!x.take_calls().iter().any(|c| matches!(c, Call::SetOwner { .. })));
}

fn save_request(x: &mut FakeX, requestor: Window, property: Option<&str>, time: u32) -> Event {
    Event::SelectionRequest {
        requestor,
        selection: x.a("CLIPBOARD_MANAGER"),
        target: x.a("SAVE_TARGETS"),
        property: property.map(|p| x.a(p)),
        time: Timestamp(time),
    }
}

#[test]
fn save_targets_from_an_app_that_owns_only_primary_takes_nothing() {
    let mut x = FakeX::new();
    let mut e = firefox_copies(&mut x, "firefox's", 300);
    x.take_calls();
    let r = send!(e, &mut x, save_request(&mut x, QT_APP, Some("_QT_SELECTION"), 500), Millis(10));
    assert_eq!(r, vec![Report::SaveAnswered { taken: false }]);
    let clipboard = x.a("CLIPBOARD");
    assert_eq!(x.owners.get(&clipboard).and_then(|o| o.0), Some(FIREFOX));
    let calls = x.take_calls();
    let qt = x.a("_QT_SELECTION");
    assert!(calls.iter().any(|c| matches!(c, Call::Notify { requestor, property: Some(p), .. } if *requestor == QT_APP && *p == qt)));
    assert!(!calls.iter().any(|c| matches!(c, Call::SetOwner { .. } | Call::Convert { .. })));
}

#[test]
fn save_targets_from_the_owner_fetches_what_it_lists_but_never_delete_then_takes_over() {
    let mut x = FakeX::new();
    let mut e = firefox_copies(&mut x, "kept", 300);
    x.take_calls();
    let list = x.atom_list(&["UTF8_STRING", "DELETE", "application/x-office"]);
    let save = x.a("SAVE_LIST");
    x.put(FIREFOX, save, "ATOM", Format::Bits32, &list);
    let r = send!(e, &mut x, save_request(&mut x, FIREFOX, Some("SAVE_LIST"), 500), Millis(10));
    assert_eq!(r, vec![]);
    let (target, p) = one(x.converts());
    assert_eq!(
        target, "application/x-office",
        "UTF8_STRING is already held, DELETE is never converted"
    );
    let r = send!(
        e,
        &mut x,
        answer(&mut x, "application/x-office", p, "application/x-office", Format::Bits8, b"doc"),
        Millis(11),
    );
    assert!(r.contains(&Report::SaveAnswered { taken: true }));
    let (content, _) = some(captured(&r));
    assert!(content.kept("application/x-office").is_some());
    let clipboard = x.a("CLIPBOARD");
    assert_eq!(x.owners.get(&clipboard), Some(&(Some(OURS), Timestamp(500))));
    let calls = x.take_calls();
    let null = x.a("NULL");
    assert!(calls.iter().any(|c| matches!(c, Call::Change { window, property, type_, data, .. } if *window == FIREFOX && *property == save && *type_ == null && data.is_empty())));
    assert!(calls.iter().any(|c| matches!(c, Call::Notify { requestor, property: Some(p), .. } if *requestor == FIREFOX && *p == save)));
}

#[test]
fn save_targets_does_not_fetch_again_the_text_names_served_from_the_kept_text() {
    let mut x = FakeX::new();
    let mut e = firefox_copies(&mut x, "kept", 300);
    x.take_calls();
    let list = x.atom_list(&[
        "UTF8_STRING",
        "STRING",
        "TEXT",
        "COMPOUND_TEXT",
        "text/plain",
        "text/plain;charset=utf-8",
        "application/x-office",
    ]);
    let save = x.a("SAVE_LIST");
    x.put(FIREFOX, save, "ATOM", Format::Bits32, &list);
    send!(e, &mut x, save_request(&mut x, FIREFOX, Some("SAVE_LIST"), 500), Millis(10));
    let (target, p) = one(x.converts());
    assert_eq!(target, "application/x-office");
    let r = send!(
        e,
        &mut x,
        answer(&mut x, "application/x-office", p, "application/x-office", Format::Bits8, b"doc"),
        Millis(11),
    );
    assert!(r.contains(&Report::SaveAnswered { taken: true }), "nothing else asked for");
    let (content, _) = some(captured(&r));
    assert!(content.kept("STRING").is_none());
}

/// Firefox offers `offered`; returns the engine with the first conversion after `TARGETS` still
/// outstanding, and that conversion's target and property.
fn offering(x: &mut FakeX, offered: &[&str]) -> (jackstay_selection::Engine, String, Atom) {
    let (mut e, _) = engine(x);
    x.take_calls();
    send!(e, x, set(x, Some(FIREFOX), 300), Millis(0));
    let (_, tp) = one(x.converts());
    let list = x.atom_list(offered);
    send!(e, x, answer(x, "TARGETS", tp, "ATOM", Format::Bits32, &list), Millis(1));
    let (target, p) = one(x.converts());
    (e, target, p)
}

/// Asks the engine to save `listed`, and answers each conversion it then makes, in order.
fn save_and_fetch(
    x: &mut FakeX,
    e: &mut jackstay_selection::Engine,
    listed: &[&str],
) -> Vec<String> {
    let list = x.atom_list(listed);
    let save = x.a("SAVE_LIST");
    x.put(FIREFOX, save, "ATOM", Format::Bits32, &list);
    send!(e, x, save_request(x, FIREFOX, Some("SAVE_LIST"), 500), Millis(10));
    let mut fetched = Vec::new();
    let mut at = 11;
    while let [(t, p)] = x.converts().as_slice() {
        let (t, p) = (t.clone(), *p);
        send!(e, x, answer(x, &t, p, &t, Format::Bits8, b"t"), Millis(at));
        fetched.push(t);
        at += 1;
    }
    fetched
}

#[test]
fn without_a_text_kept_save_targets_fetches_the_text_names_it_lists() {
    // An image: no text held, so the owner's own text names are the only text there is.
    let mut x = FakeX::new();
    let (mut e, first, p) = offering(&mut x, &["image/png", "STRING", "TEXT"]);
    assert_eq!(first, "STRING", "STRING is the text when it is all there is");
    send!(e, &mut x, refuse(&mut x, "STRING"), Millis(2));
    let (png, p2) = one(x.converts());
    assert_eq!(png, "image/png");
    let _ = p;
    send!(
        e,
        &mut x,
        answer(&mut x, "image/png", p2, "image/png", Format::Bits8, b"png"),
        Millis(3)
    );
    // The text was refused, so none is kept: TEXT, asked for again at SAVE_TARGETS, is fetched.
    assert_eq!(save_and_fetch(&mut x, &mut e, &["image/png", "TEXT"]), ["TEXT"]);
}

#[test]
fn a_save_targets_while_the_text_is_being_fetched_does_not_fetch_its_other_names() {
    let mut x = FakeX::new();
    let (mut e, first, p) = offering(&mut x, &["UTF8_STRING", "STRING", "application/x-doc"]);
    assert_eq!(first, "UTF8_STRING");
    // SAVE_TARGETS arrives with UTF8_STRING still outstanding.
    let list = x.atom_list(&["UTF8_STRING", "STRING", "application/x-doc"]);
    let save = x.a("SAVE_LIST");
    x.put(FIREFOX, save, "ATOM", Format::Bits32, &list);
    send!(e, &mut x, save_request(&mut x, FIREFOX, Some("SAVE_LIST"), 500), Millis(2));
    assert_eq!(x.converts(), vec![]);
    send!(
        e,
        &mut x,
        answer(&mut x, "UTF8_STRING", p, "UTF8_STRING", Format::Bits8, b"t"),
        Millis(3)
    );
    let (next, p2) = one(x.converts());
    assert_eq!(next, "application/x-doc", "not STRING");
    send!(
        e,
        &mut x,
        answer(&mut x, "application/x-doc", p2, "application/x-doc", Format::Bits8, b"d"),
        Millis(4)
    );
    assert_eq!(x.converts(), vec![]);
}

#[test]
fn save_targets_with_no_property_saves_everything_and_replies_on_save_targets() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    send!(e, &mut x, set(&mut x, Some(FIREFOX), 300), Millis(0));
    let (_, tp) = one(x.converts());
    let list = x.atom_list(&["UTF8_STRING", "application/x-office", "DELETE"]);
    send!(e, &mut x, answer(&mut x, "TARGETS", tp, "ATOM", Format::Bits32, &list), Millis(1));
    let (_, p) = one(x.converts());
    send!(
        e,
        &mut x,
        answer(&mut x, "UTF8_STRING", p, "UTF8_STRING", Format::Bits8, b"t"),
        Millis(2),
    );
    send!(e, &mut x, save_request(&mut x, FIREFOX, None, 0), Millis(3));
    let (target, p) = one(x.converts());
    assert_eq!(target, "application/x-office");
    let r = send!(
        e,
        &mut x,
        answer(&mut x, "application/x-office", p, "application/x-office", Format::Bits8, b"d"),
        Millis(4),
    );
    assert!(r.contains(&Report::SaveAnswered { taken: true }));
    let save_targets = x.a("SAVE_TARGETS");
    assert!(
        x.take_calls()
            .iter()
            .any(|c| matches!(c, Call::Notify { property: Some(p), .. } if *p == save_targets))
    );
}

#[test]
fn save_targets_is_answered_within_its_budget_with_what_is_held() {
    let mut x = FakeX::new();
    let mut e = firefox_copies(&mut x, "kept", 300);
    let list = x.atom_list(&["application/x-slow"]);
    let save = x.a("SAVE_LIST");
    x.put(FIREFOX, save, "ATOM", Format::Bits32, &list);
    send!(e, &mut x, save_request(&mut x, FIREFOX, Some("SAVE_LIST"), 500), Millis(1_000));
    assert_eq!(one(x.converts()).0, "application/x-slow");
    assert_eq!(e.tick(&mut x, Millis(4_999)), vec![]);
    let r = e.tick(&mut x, Millis(5_000));
    assert_eq!(r, vec![Report::SaveAnswered { taken: true }]);
}

#[test]
fn targets_lists_held_targets_and_text_aliases() {
    let mut x = FakeX::new();
    let mut e = owning(&mut x, "café");
    send!(e, &mut x, request(&mut x, XTERM, "TARGETS", "P", 3_000), Millis(1));
    let (_, data) = some(written(&mut x, XTERM, "P"));
    let names: Vec<String> =
        data.as_chunks::<4>().0.iter().map(|b| x.name_of(Atom(u32::from_ne_bytes(*b)))).collect();
    for n in [
        "TARGETS",
        "MULTIPLE",
        "TIMESTAMP",
        "UTF8_STRING",
        "STRING",
        "TEXT",
        "COMPOUND_TEXT",
        "text/plain;charset=utf-8",
    ] {
        assert!(names.iter().any(|m| m == n), "{n} missing from {names:?}");
    }
    assert!(!names.iter().any(|m| m == "text/plain"), "not ASCII");
}

#[test]
fn text_aliases_are_served_in_their_own_encodings() {
    let mut x = FakeX::new();
    let mut e = owning(&mut x, "café");
    send!(e, &mut x, request(&mut x, XTERM, "STRING", "P", 0), Millis(1));
    assert_eq!(written(&mut x, XTERM, "P"), Some(("STRING".into(), b"caf\xe9".to_vec())));
    send!(e, &mut x, request(&mut x, XTERM, "TEXT", "P", 0), Millis(2));
    assert_eq!(written(&mut x, XTERM, "P"), Some(("STRING".into(), b"caf\xe9".to_vec())));
    send!(e, &mut x, request(&mut x, XTERM, "text/plain", "P", 0), Millis(3));
    assert_eq!(notified(&mut x), Some(Notice::Refused), "text/plain is ASCII only");

    let mut x = FakeX::new();
    let mut e = owning(&mut x, "→");
    send!(e, &mut x, request(&mut x, XTERM, "STRING", "P", 0), Millis(1));
    assert_eq!(notified(&mut x), Some(Notice::Refused), "not Latin-1, so STRING is refused");
    send!(e, &mut x, request(&mut x, XTERM, "TEXT", "P", 0), Millis(2));
    assert_eq!(written(&mut x, XTERM, "P"), Some(("COMPOUND_TEXT".into(), compound_text("→"))));
}

#[test]
fn requests_from_before_we_owned_it_are_refused_and_current_time_is_accepted() {
    let mut x = FakeX::new();
    let mut e = owning(&mut x, "hi");
    send!(e, &mut x, request(&mut x, XTERM, "UTF8_STRING", "P", 1_500), Millis(1));
    assert_eq!(notified(&mut x), Some(Notice::Refused));
    send!(e, &mut x, request(&mut x, XTERM, "UTF8_STRING", "P", 0), Millis(2));
    assert!(matches!(notified(&mut x), Some(Notice::Sent(_))));
}

#[test]
fn an_obsolete_client_without_a_property_gets_the_target_as_property() {
    let mut x = FakeX::new();
    let mut e = owning(&mut x, "hi");
    let ev = Event::SelectionRequest {
        requestor: XTERM,
        selection: x.a("CLIPBOARD"),
        target: x.a("UTF8_STRING"),
        property: None,
        time: Timestamp(0),
    };
    send!(e, &mut x, ev, Millis(1));
    assert_eq!(written(&mut x, XTERM, "UTF8_STRING").map(|w| w.1), Some(b"hi".to_vec()));
}

#[test]
fn multiple_serves_each_pair_and_marks_failures_none() {
    let mut x = FakeX::new();
    let mut e = owning(&mut x, "hi");
    let pairs: Vec<u8> = [("UTF8_STRING", "A"), ("image/png", "B")]
        .iter()
        .flat_map(|(t, p)| [x.a(t).0.to_ne_bytes(), x.a(p).0.to_ne_bytes()].concat())
        .collect();
    let list = x.a("LIST");
    x.put(XTERM, list, "ATOM_PAIR", Format::Bits32, &pairs);
    send!(e, &mut x, request(&mut x, XTERM, "MULTIPLE", "LIST", 0), Millis(1));
    assert_eq!(written(&mut x, XTERM, "A").map(|w| w.1), Some(b"hi".to_vec()));
    let (type_, back) = some(written(&mut x, XTERM, "LIST"));
    assert_eq!(type_, "ATOM_PAIR");
    let atoms: Vec<u32> = back.as_chunks::<4>().0.iter().map(|b| u32::from_ne_bytes(*b)).collect();
    assert_eq!(atoms, vec![x.a("UTF8_STRING").0, x.a("A").0, x.a("image/png").0, 0]);
    assert_eq!(notified(&mut x), Some(Notice::Sent(list)));
}

#[test]
fn large_replies_go_by_incr_and_a_slow_requestor_stalls_only_itself() {
    let mut x = FakeX::new();
    let big = "x".repeat(600 * 1024);
    let mut e = owning(&mut x, &big);
    send!(e, &mut x, request(&mut x, XTERM, "UTF8_STRING", "P", 0), Millis(1));
    let p = x.a("P");
    let calls = x.take_calls();
    assert!(calls.contains(&Call::Watch { window: XTERM, on: true }));
    assert_eq!(written(&mut x, XTERM, "P").map(|w| w.0), Some("INCR".into()));
    // Meanwhile another requestor is served at once.
    send!(e, &mut x, request(&mut x, FIREFOX, "TARGETS", "Q", 0), Millis(2));
    assert!(matches!(notified(&mut x), Some(Notice::Sent(_))));
    let mut received = Vec::new();
    for i in 0..10 {
        send!(e, &mut x, deleted(XTERM, p), Millis(3 + i));
        let (_, chunk) = some(written(&mut x, XTERM, "P"));
        if chunk.is_empty() {
            break;
        }
        received.extend(chunk);
    }
    assert_eq!(received.len(), big.len());
    send!(e, &mut x, deleted(XTERM, p), Millis(20));
    assert!(x.take_calls().contains(&Call::Watch { window: XTERM, on: false }));
    assert_eq!(e.next_deadline(), None);
}

#[test]
fn an_incr_transfer_is_dropped_when_its_requestor_goes() {
    let mut x = FakeX::new();
    let mut e = owning(&mut x, &"x".repeat(600 * 1024));
    send!(e, &mut x, request(&mut x, XTERM, "UTF8_STRING", "P", 0), Millis(1));
    assert_eq!(e.next_deadline(), Some(Millis(10_001)));
    send!(e, &mut x, Event::DestroyNotify { window: XTERM }, Millis(2));
    assert_eq!(e.next_deadline(), None);
    let mut x2 = FakeX::new();
    let mut e2 = owning(&mut x2, &"x".repeat(600 * 1024));
    send!(e2, &mut x2, request(&mut x2, XTERM, "UTF8_STRING", "P", 0), Millis(1));
    e2.tick(&mut x2, Millis(10_001));
    assert_eq!(e2.next_deadline(), None);
    assert!(x2.take_calls().contains(&Call::Watch { window: XTERM, on: false }));
}

#[test]
fn motif_never_gets_incr() {
    let mut x = FakeX::new();
    let mut e = owning(&mut x, &"x".repeat(600 * 1024));
    send!(e, &mut x, request(&mut x, XTERM, "UTF8_STRING", "CLIP_TEMPORARY", 0), Millis(1));
    assert_eq!(notified(&mut x), Some(Notice::Refused));
}

#[test]
fn clear_abandons_the_capture_claims_empty_and_empties_cut_buffers() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    send!(e, &mut x, set(&mut x, Some(FIREFOX), 300), Millis(0));
    let (_, tp) = one(x.converts());
    let r = e.clear(&mut x, Millis(1));
    assert_eq!(r, vec![Report::Claimed]);
    assert!(x.take_calls().contains(&Call::ClearCutBuffers));
    // The password manager's reply arrives after the clear: it must not come back.
    let list = x.atom_list(&["UTF8_STRING"]);
    assert_eq!(
        send!(e, &mut x, answer(&mut x, "TARGETS", tp, "ATOM", Format::Bits32, &list), Millis(2)),
        vec![]
    );
    assert_eq!(x.converts(), vec![]);
    send!(e, &mut x, request(&mut x, XTERM, "UTF8_STRING", "P", 0), Millis(3));
    assert_eq!(notified(&mut x), Some(Notice::Refused), "a cleared clipboard pastes nothing");
    send!(e, &mut x, request(&mut x, XTERM, "TARGETS", "P", 0), Millis(4));
    let (_, data) = some(written(&mut x, XTERM, "P"));
    assert_eq!(data.len(), 8, "only TARGETS and TIMESTAMP");
}

#[test]
fn losing_the_clipboard_is_reported_and_stops_serving() {
    let mut x = FakeX::new();
    let mut e = owning(&mut x, "hi");
    let clear = Event::SelectionClear { selection: x.a("CLIPBOARD"), time: Timestamp(5_000) };
    assert_eq!(send!(e, &mut x, clear, Millis(1)), vec![Report::Lost]);
    send!(e, &mut x, request(&mut x, XTERM, "UTF8_STRING", "P", 0), Millis(2));
    assert_eq!(notified(&mut x), Some(Notice::Refused));
}

#[test]
fn a_hung_owner_is_reported_after_two_unanswered_probes() {
    let mut x = FakeX::new();
    let mut e = firefox_copies(&mut x, "hi", 300);
    x.take_calls();
    let mut reported_at = None;
    for _ in 0..6 {
        let now = some(e.next_deadline());
        if e.tick(&mut x, now).contains(&Report::OwnerHung) && reported_at.is_none() {
            reported_at = Some(now);
        }
    }
    // Probes at 30 s and 70 s, each given 10 s to answer.
    assert_eq!(reported_at, Some(Millis(80_000)));
}

#[test]
fn an_answered_probe_resets_the_count() {
    let mut x = FakeX::new();
    let mut e = firefox_copies(&mut x, "hi", 300);
    x.take_calls();
    for _ in 0..5 {
        let now = some(e.next_deadline());
        let r = e.tick(&mut x, now);
        assert!(!r.contains(&Report::OwnerHung));
        if let Some((_, p)) = x.converts().pop() {
            let list = x.atom_list(&["UTF8_STRING"]);
            let r =
                send!(e, &mut x, answer(&mut x, "TARGETS", p, "ATOM", Format::Bits32, &list), now);
            assert_eq!(r, vec![], "a probe's answer is not a new capture");
        }
    }
}

#[test]
fn our_own_claims_are_not_captured() {
    let mut x = FakeX::new();
    let mut e = owning(&mut x, "hi");
    let r = send!(e, &mut x, set(&mut x, Some(OURS), 2_001), Millis(1));
    assert_eq!(r, vec![]);
    assert_eq!(x.converts(), vec![]);
}

#[test]
fn timestamps_compare_across_wraparound() {
    assert!(Timestamp(5).is_earlier_than(Timestamp(6)));
    assert!(!Timestamp(6).is_earlier_than(Timestamp(5)));
    assert!(Timestamp(u32::MAX).is_earlier_than(Timestamp(3)));
    assert!(!Timestamp(7).is_earlier_than(Timestamp(7)));
    assert_eq!(latin1("é→"), None);
    assert_eq!(compound_text("é"), b"\xe9".to_vec());
    assert_eq!(compound_text("→"), [b"\x1b%G".as_slice(), "→".as_bytes(), b"\x1b%@"].concat());
    let _ = OwnerChange::WindowDestroyed;
}

// A probe that comes due while a capture is still talking to the owner moves on; it never stays
// due.
#[test]
fn a_probe_due_during_a_capture_is_rescheduled_not_left_due() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    send!(e, &mut x, set(&mut x, Some(XTERM), 0), Millis(0));
    let mut last = Millis(0);
    for _ in 0..8 {
        let Some(d) = e.next_deadline() else { break };
        assert!(d > last || d == Millis(0), "deadline stuck at {d}");
        e.tick(&mut x, d);
        last = d;
    }
}

fn primary_request(x: &mut FakeX, target: &str, property: &str) -> Event {
    Event::SelectionRequest {
        requestor: XTERM,
        selection: x.a("PRIMARY"),
        target: x.a(target),
        property: Some(x.a(property)),
        time: Timestamp(0),
    }
}

#[test]
fn clear_claims_primary_too_offering_nothing() {
    let mut x = FakeX::new();
    let mut e = owning(&mut x, "on the clipboard");
    x.claim("PRIMARY", XTERM, 1_500);
    x.time = 3_000;
    assert_eq!(e.clear(&mut x, Millis(1)), vec![Report::Claimed]);
    let primary = x.a("PRIMARY");
    // The fake's server time steps on each query.
    assert_eq!(x.owners.get(&primary).copied(), Some((Some(OURS), Timestamp(3_001))));
    x.take_calls();
    send!(e, &mut x, primary_request(&mut x, "UTF8_STRING", "P"), Millis(2));
    assert_eq!(notified(&mut x), Some(Notice::Refused), "a cleared PRIMARY pastes nothing");
    send!(e, &mut x, primary_request(&mut x, "TARGETS", "P"), Millis(3));
    assert_eq!(some(written(&mut x, XTERM, "P")).1.len(), 8, "only TARGETS and TIMESTAMP");
    // A SelectionClear from before the clear's claim is stale; a later one ends it.
    let stale = Event::SelectionClear { selection: primary, time: Timestamp(3_000) };
    assert_eq!(send!(e, &mut x, stale, Millis(4)), vec![]);
    x.props.clear();
    send!(e, &mut x, primary_request(&mut x, "TARGETS", "P"), Millis(5));
    assert!(written(&mut x, XTERM, "P").is_some(), "still held after a stale clear");
    let lost = Event::SelectionClear { selection: primary, time: Timestamp(3_001) };
    assert_eq!(send!(e, &mut x, lost, Millis(6)), vec![], "PRIMARY is not reported");
    x.props.clear();
    x.take_calls();
    send!(e, &mut x, primary_request(&mut x, "TARGETS", "P"), Millis(7));
    assert_eq!(notified(&mut x), Some(Notice::Refused));
    assert!(written(&mut x, XTERM, "P").is_none());
    assert!(e.owns_clipboard(), "losing PRIMARY leaves CLIPBOARD alone");
}

#[test]
fn primary_is_never_served_the_clipboard() {
    let mut x = FakeX::new();
    let mut e = owning(&mut x, "on the clipboard");
    send!(e, &mut x, primary_request(&mut x, "TARGETS", "P"), Millis(1));
    assert_eq!(notified(&mut x), Some(Notice::Refused));
    assert!(written(&mut x, XTERM, "P").is_none());
}

#[test]
fn a_clear_whose_primary_claim_fails_holds_nothing() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.time = 3_000;
    // A client that claimed PRIMARY after the clear took its time keeps it.
    x.claim("PRIMARY", XTERM, 3_005);
    assert_eq!(e.clear(&mut x, Millis(1)), vec![Report::Claimed]);
    x.take_calls();
    send!(e, &mut x, primary_request(&mut x, "TARGETS", "P"), Millis(2));
    assert_eq!(notified(&mut x), Some(Notice::Refused));
}

#[test]
fn every_capture_is_announced_before_its_content() {
    let mut x = FakeX::new();
    x.claim("CLIPBOARD", FIREFOX, 5);
    let (mut e, reports) = engine(&mut x);
    assert_eq!(reports, vec![Report::CaptureStarted { capture: 1 }], "the owner at start");
    let r = send!(e, &mut x, set(&mut x, Some(QT_APP), 10), Millis(1));
    assert_eq!(r, vec![Report::CaptureStarted { capture: 2 }]);
    // Our own claim, and a release, start nothing.
    x.time = 20;
    assert_eq!(
        e.serve(&mut x, crate::support::text_content("h"), Millis(2)),
        vec![Report::Claimed]
    );
    let r = send!(e, &mut x, set(&mut x, Some(OURS), 20), Millis(3));
    assert_eq!(r, vec![]);
    let r = send!(e, &mut x, set(&mut x, None, 30), Millis(4));
    assert_eq!(r, vec![]);
}

#[test]
fn restore_serves_only_an_unowned_clipboard() {
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.time = 1_000;
    x.take_calls();
    assert_eq!(
        e.restore(&mut x, crate::support::text_content("saved"), Millis(1)),
        vec![Report::Claimed]
    );
    assert!(e.owns_clipboard());
    let clipboard = x.a("CLIPBOARD");
    assert_eq!(x.owners.get(&clipboard).copied(), Some((Some(OURS), Timestamp(1_001))));
    send!(e, &mut x, request(&mut x, XTERM, "UTF8_STRING", "P", 0), Millis(2));
    assert_eq!(written(&mut x, XTERM, "P").map(|w| w.1), Some(b"saved".to_vec()));

    let calls = x.take_calls();
    let grab = calls.iter().position(|c| *c == Call::Grab(true));
    let claim = calls.iter().position(|c| matches!(c, Call::SetOwner { .. }));
    let ungrab = calls.iter().position(|c| *c == Call::Grab(false));
    assert!(grab.is_some() && grab < claim && claim < ungrab, "claimed under a grab: {calls:?}");

    // An application that owns the clipboard keeps it, even one the engine has not heard of yet.
    for already_seen in [true, false] {
        let mut x = FakeX::new();
        let (mut e, _) = engine(&mut x);
        x.time = 1_000;
        if already_seen {
            send!(e, &mut x, set(&mut x, Some(FIREFOX), 900), Millis(0));
        } else {
            x.claim("CLIPBOARD", FIREFOX, 900);
        }
        x.take_calls();
        let r = e.restore(&mut x, crate::support::text_content("saved"), Millis(1));
        assert_eq!(r, vec![Report::ClaimFailed]);
        assert!(!x.take_calls().iter().any(|c| matches!(c, Call::SetOwner { .. })));
        let clipboard = x.a("CLIPBOARD");
        assert_eq!(x.owners.get(&clipboard).map(|o| o.0), Some(Some(FIREFOX)));
    }

    // A client claiming while the restore checks and claims is held off by the grab, and its claim,
    // later than ours, stands.
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.time = 1_000;
    x.racing_claim = Some(FIREFOX);
    x.take_calls();
    let r = e.restore(&mut x, crate::support::text_content("saved"), Millis(1));
    assert_eq!(r, vec![Report::ClaimFailed]);
    let clipboard = x.a("CLIPBOARD");
    assert_eq!(x.owners.get(&clipboard).map(|o| o.0), Some(Some(FIREFOX)));
    assert!(!e.owns_clipboard());
    let calls = x.take_calls();
    assert_eq!(calls.first(), Some(&Call::Grab(true)));
    assert_eq!(calls.iter().filter(|c| matches!(c, Call::Grab(false))).count(), 1);

    // Nor does a restore replace our own claim.
    let mut x = FakeX::new();
    let mut e = owning(&mut x, "newer");
    let r = e.restore(&mut x, crate::support::text_content("saved"), Millis(1));
    assert_eq!(r, vec![Report::ClaimFailed]);
    send!(e, &mut x, request(&mut x, XTERM, "UTF8_STRING", "P", 0), Millis(2));
    assert_eq!(written(&mut x, XTERM, "P").map(|w| w.1), Some(b"newer".to_vec()));
}

#[test]
fn debug_output_never_holds_clipboard_bytes() {
    // Mid-capture by INCR: the received chunk sits in the capture.
    let mut x = FakeX::new();
    let (mut e, _) = engine(&mut x);
    x.take_calls();
    send!(e, &mut x, set(&mut x, Some(FIREFOX), 9), Millis(0));
    let (_, tp) = one(x.converts());
    let list = x.atom_list(&["UTF8_STRING"]);
    send!(e, &mut x, answer(&mut x, "TARGETS", tp, "ATOM", Format::Bits32, &list), Millis(1));
    let (_, p) = one(x.converts());
    let incr = answer(&mut x, "UTF8_STRING", p, "INCR", Format::Bits32, &10u32.to_ne_bytes());
    send!(e, &mut x, incr, Millis(2));
    x.put(OURS, p, "UTF8_STRING", Format::Bits8, b"hunter2-a");
    send!(e, &mut x, new_value(p), Millis(3));
    let printed = format!("{e:?}");
    assert!(printed.contains("Incr {"), "the test reaches an INCR capture: {printed}");
    assert!(printed.contains("len: 9, deadline"), "{printed}");
    assert!(!printed.contains("hunter2") && !printed.contains("104, 117"), "{printed}");

    // Serving, with an outgoing INCR transfer under way.
    let mut x = FakeX::new();
    let mut e = owning(&mut x, &"hunter2-b ".repeat(60_000));
    send!(e, &mut x, request(&mut x, XTERM, "UTF8_STRING", "P", 0), Millis(1));
    let printed = format!("{e:?}");
    assert!(printed.contains("Transfer"), "the test reaches a transfer: {printed}");
    // Lengths stand in for the bytes, so the output still says what is held.
    assert!(printed.contains("Served { since:"), "{}", printed.len());
    assert!(printed.contains("text_len: Some(600000)"), "{}", printed.len());
    assert!(printed.contains("value: Value { type_: Atom("), "{}", printed.len());
    assert!(printed.contains("len: 600000 }"), "{}", printed.len());
    assert!(!printed.contains("hunter2") && !printed.contains("104, 117"), "{}", printed.len());

    let read = jackstay_selection::Property {
        type_: Atom(1),
        format: Format::Bits8,
        data: b"hunter2-c".to_vec(),
    };
    assert_eq!(format!("{read:?}"), "Property { type_: Atom(1), format: Bits8, len: 9 }");
}
