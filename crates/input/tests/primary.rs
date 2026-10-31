//! The primary's state machine (docs/input.md, "Primary"), driven by scripted sequences: what X
//! and the link report, and what it does and sends.

use std::time::{Duration, Instant};

use jackstay_input::{
    ANSWER_WITHIN, Action, Do, Hotkey, Keymap, Layout, Primary, RETRY_UNANSWERED, Secondary, Sides,
    Where,
};
use jackstay_proto::link::{KeyEntry, Message, Monitor, Side};
use proptest::prelude::*;

#[track_caller]
fn layout(monitors: &[(u16, u16, u16, u16)]) -> Layout {
    let monitors: Vec<Monitor> =
        monitors.iter().map(|&(x, y, width, height)| Monitor { x, y, width, height }).collect();
    Layout::new(&monitors).unwrap_or_else(|e| panic!("{e:?}"))
}

/// athena: four 2560×1440 monitors in a 2×2 grid.
fn athena() -> Layout {
    layout(&[
        (2560, 0, 2560, 1440),
        (0, 0, 2560, 1440),
        (0, 1440, 2560, 1440),
        (2560, 1440, 2560, 1440),
    ])
}

/// xiphos: a 1920×1200 monitor above two 3440×1440, with dead zones either side of it.
fn xiphos() -> Layout {
    layout(&[(0, 1200, 3440, 1440), (939, 0, 1920, 1200), (0, 2640, 3440, 1440)])
}

const XIPHOS: u8 = 1;
const OTHER: u8 = 2;
const SESSION: u64 = 7;
const SHIFT: u8 = 50;
const SCROLL_LOCK: u8 = 78;
const KEY_A: u8 = 38;

/// athena's keymap, as far as these tests need it: Shift, Scroll Lock and `a`.
fn keymap() -> (u8, Vec<KeyEntry>) {
    let mut keys = vec![KeyEntry { level1: 0, level2: 0, modifier: false }; 248];
    let mut set = |code: u8, sym: u32, modifier: bool| {
        keys[usize::from(code - 8)] = KeyEntry { level1: sym, level2: sym, modifier };
    };
    set(SHIFT, 0xffe1, true);
    set(SCROLL_LOCK, 0xff14, false);
    set(KEY_A, 0x61, false);
    (8, keys)
}

/// athena as the owner has it: xiphos beyond both sides; Scroll Lock returns, Shift+Scroll Lock
/// locks.
fn athena_primary() -> Primary<u8> {
    Primary::new(
        athena(),
        [Some(XIPHOS), Some(XIPHOS), None, None],
        keymap(),
        Some(Hotkey { sym: 0xff14, shift: false }),
        Some(Hotkey { sym: 0xff14, shift: true }),
    )
}

/// What `doing` sends, in order.
fn sent(doing: &[Do<u8>]) -> Vec<&Message> {
    doing
        .iter()
        .filter_map(|d| if let Do::Send { message, .. } = d { Some(message) } else { None })
        .collect()
}

#[track_caller]
fn nothing(out: &[Do<u8>]) {
    assert_eq!(out, []);
}

fn shift_held() -> [u8; 32] {
    let mut modifiers = [0u8; 32];
    modifiers[usize::from(SHIFT / 8)] |= 1 << (SHIFT % 8);
    modifiers
}

/// athena with a session to xiphos kept and its layout known: ready to cross.
fn ready() -> (Primary<u8>, Instant) {
    let mut p = athena_primary();
    // A primary sends no layout: a peer's says it is a secondary.
    nothing(&p.kept(XIPHOS, SESSION));
    p.message(XIPHOS, SESSION, &Message::Layout { monitors: xiphos().wire() });
    (p, Instant::now())
}

/// Crossed through athena's left side at `y`: the span is 1.
fn crossed(y: i32) -> (Primary<u8>, Instant) {
    let (mut p, now) = ready();
    assert_eq!(p.barrier(Side::Left, 0, y, -256, 0, now), vec![Do::Grab]);
    p.grabbed(true, [0; 32], 0, now);
    (p, now)
}

#[test]
fn barriers_lie_on_the_sides_with_a_neighbour() {
    let p = athena_primary();
    let Do::Barriers(edges) = p.barriers() else { panic!() };
    let sides: Vec<Side> = edges.iter().map(|e| e.side).collect();
    assert_eq!(sides, vec![Side::Left, Side::Right]);
}

#[test]
fn no_crossing_until_the_peer_is_linked_and_its_layout_known() {
    let mut p = athena_primary();
    let now = Instant::now();
    nothing(&p.barrier(Side::Left, 0, 100, -256, 0, now));
    p.kept(XIPHOS, SESSION);
    nothing(&p.barrier(Side::Left, 0, 100, -256, 0, now));
    // A layout on another session changes nothing.
    p.message(XIPHOS, SESSION + 1, &Message::Layout { monitors: xiphos().wire() });
    nothing(&p.barrier(Side::Left, 0, 100, -256, 0, now));
    p.message(XIPHOS, SESSION, &Message::Layout { monitors: xiphos().wire() });
    // Pushing along the barrier, or inward, is no crossing.
    nothing(&p.barrier(Side::Left, 0, 100, 0, 256, now));
    nothing(&p.barrier(Side::Left, 0, 100, 256, 0, now));
    assert_eq!(p.barrier(Side::Left, 0, 100, -256, 0, now), vec![Do::Grab]);
    assert_eq!(p.position(), Where::Crossing(XIPHOS));
}

#[test]
fn a_crossing_sends_the_keymap_once_then_enter_and_parks_the_pointer() {
    let (mut p, now) = ready();
    p.barrier(Side::Left, 0, 2000, -256, 0, now);
    let modifiers = shift_held();
    let out = p.grabbed(true, modifiers, 1, now);
    let fraction = athena().edge(Side::Left).fraction(2000);
    // The exit ends a drain the secondary may be in after a silence only it noticed.
    assert!(
        matches!(
            sent(&out)[..],
            [Message::Keymap { first: 8, .. }, Message::Exit { span: 0 }, Message::Enter { .. }]
        ),
        "{out:?}"
    );
    assert_eq!(
        sent(&out)[2],
        &Message::Enter { span: 1, side: Side::Right, fraction, modifiers, locks: 1 }
    );
    // Parked at the middle of the monitor it crossed from; the session pings faster.
    assert!(out.contains(&Do::Warp { x: 1280, y: 2160 }), "{out:?}");
    assert_eq!(out.last(), Some(&Do::Active { peer: XIPHOS, on: true }));
    assert_eq!(p.position(), Where::On { peer: XIPHOS, locked: false });
    // Back by the return key (Shift up first: with it, Scroll Lock locks), and across again: no
    // second keymap on the same session.
    p.key(SHIFT, false);
    p.key(SCROLL_LOCK, true);
    assert_eq!(p.position(), Where::Here);
    p.barrier(Side::Left, 0, 2000, -256, 0, now + Duration::from_secs(1));
    let out = p.grabbed(true, [0; 32], 0, now);
    assert!(
        matches!(sent(&out)[..], [Message::Exit { span: 1 }, Message::Enter { span: 2, .. }]),
        "{out:?}"
    );
}

#[test]
fn a_refused_grab_stays_here_and_attempts_are_limited() {
    let (mut p, now) = ready();
    assert_eq!(p.barrier(Side::Left, 0, 100, -256, 0, now), vec![Do::Grab]);
    nothing(&p.grabbed(false, [0; 32], 0, now));
    assert_eq!(p.position(), Where::Here);
    // Hundreds of hits a second while a menu holds a grab: one attempt per 50 ms.
    nothing(&p.barrier(Side::Left, 0, 100, -256, 0, now + Duration::from_millis(49)));
    let later = now + Duration::from_millis(50);
    assert_eq!(p.barrier(Side::Left, 0, 100, -256, 0, later), vec![Do::Grab]);
}

#[test]
fn motion_is_sent_and_the_hidden_pointer_kept_from_the_edges() {
    let (mut p, _) = crossed(2000);
    let out = p.motion(300, -5);
    let motion = Message::Motion { span: 1, dx: 300, dy: -5 };
    assert_eq!(out, vec![Do::Send { peer: XIPHOS, session: SESSION, message: motion }]);
    // An eighth of the monitor's width (320 px) from the park point: warped back.
    let mut warped = 0;
    for _ in 0..400 {
        if p.motion(256, 0).contains(&Do::Warp { x: 1280, y: 2160 }) {
            warped += 1;
        }
    }
    assert_eq!(warped, 1);
}

#[test]
fn the_return_key_comes_back_where_the_pointer_crossed() {
    let (mut p, _) = crossed(2000);
    let out = p.key(KEY_A, true);
    assert_eq!(sent(&out), vec![&Message::Key { span: 1, keycode: KEY_A, down: true }]);
    let out = p.key(SCROLL_LOCK, true);
    assert_eq!(
        out,
        vec![
            Do::Send { peer: XIPHOS, session: SESSION, message: Message::Exit { span: 1 } },
            Do::Warp { x: 1, y: 2000 },
            Do::Ungrab,
            Do::Active { peer: XIPHOS, on: false },
        ]
    );
    assert_eq!(p.position(), Where::Here);
    nothing(&p.key(SCROLL_LOCK, false));
}

#[test]
fn the_lock_key_keeps_the_pointer_on_the_peer() {
    let (mut p, _) = crossed(2000);
    // Shift is sent as any key; Scroll Lock with it locks, and is not sent, nor its release.
    p.key(SHIFT, true);
    let out = p.key(SCROLL_LOCK, true);
    assert_eq!(sent(&out), vec![&Message::Lock { span: 1, on: true }]);
    nothing(&p.key(SCROLL_LOCK, false));
    assert_eq!(p.position(), Where::On { peer: XIPHOS, locked: true });
    // A leave while locked is not followed (the secondary sends none, but a late one).
    let leave = Message::Leave { span: 1, side: Side::Right, fraction: 0 };
    nothing(&p.message(XIPHOS, SESSION, &leave));
    let out = p.key(SCROLL_LOCK, true);
    assert_eq!(sent(&out), vec![&Message::Lock { span: 1, on: false }]);
    p.key(SCROLL_LOCK, false);
    p.key(SHIFT, false);
    // Scroll Lock alone now returns.
    assert!(sent(&p.key(SCROLL_LOCK, true)).contains(&&Message::Exit { span: 1 }));
}

#[test]
fn shift_held_at_the_crossing_counts_for_the_lock_key() {
    let (mut p, now) = ready();
    p.barrier(Side::Left, 0, 10, -256, 0, now);
    p.grabbed(true, shift_held(), 0, now);
    assert_eq!(sent(&p.key(SCROLL_LOCK, true)), vec![&Message::Lock { span: 1, on: true }]);
}

#[test]
fn a_leave_returns_by_the_side_facing_the_one_left_by() {
    // Out of athena's left, into xiphos's right; out of xiphos's left, back into athena's right
    // (the owner's wraparound).
    let (mut p, _) = crossed(2000);
    let fraction = xiphos().edge(Side::Left).fraction(1200 + 500);
    // A leave for another span is late: ignored.
    nothing(&p.message(XIPHOS, SESSION, &Message::Leave { span: 9, side: Side::Left, fraction }));
    let out = p.message(XIPHOS, SESSION, &Message::Leave { span: 1, side: Side::Left, fraction });
    assert_eq!(
        out,
        vec![Do::Warp { x: 5118, y: 500 }, Do::Ungrab, Do::Active { peer: XIPHOS, on: false }]
    );
    assert_eq!(p.position(), Where::Here);
}

#[test]
fn silence_ends_the_span_and_sends_exit_after_whatever_is_late() {
    let (mut p, _) = crossed(2000);
    // Another session's silence is not this one's.
    nothing(&p.suspect(XIPHOS, SESSION + 1, true));
    let out = p.suspect(XIPHOS, SESSION, true);
    assert_eq!(
        out,
        vec![
            Do::Warp { x: 1, y: 2000 },
            Do::Ungrab,
            Do::Active { peer: XIPHOS, on: false },
            Do::Send { peer: XIPHOS, session: SESSION, message: Message::Exit { span: 1 } },
        ]
    );
    // Once per silence; and no crossing until it is heard again.
    nothing(&p.suspect(XIPHOS, SESSION, true));
    let later = Instant::now() + Duration::from_secs(1);
    nothing(&p.barrier(Side::Left, 0, 10, -256, 0, later));
    p.suspect(XIPHOS, SESSION, false);
    assert_eq!(p.barrier(Side::Left, 0, 10, -256, 0, later), vec![Do::Grab]);
}

#[test]
fn silence_with_no_span_still_sends_exit_with_the_last_span() {
    let (mut p, _) = crossed(2000);
    p.key(SCROLL_LOCK, true);
    let out = p.suspect(XIPHOS, SESSION, true);
    assert_eq!(sent(&out), vec![&Message::Exit { span: 1 }]);
}

#[test]
fn a_stale_session_ends_the_span_even_if_the_link_says_nothing() {
    let (mut p, now) = crossed(2000);
    nothing(&p.freshness(Duration::from_millis(250)));
    let out = p.freshness(Duration::from_millis(400));
    assert!(out.contains(&Do::Ungrab), "{out:?}");
    assert_eq!(sent(&out), vec![&Message::Exit { span: 1 }]);
    // Not marked suspect: a link thread that stopped would never clear it.
    let later = now + Duration::from_secs(1);
    assert_eq!(p.barrier(Side::Left, 0, 10, -256, 0, later), vec![Do::Grab]);
}

#[test]
fn a_session_ended_or_replaced_brings_the_pointer_home() {
    let (mut p, now) = crossed(2000);
    let out = p.kept(XIPHOS, SESSION + 1);
    // No exit on a session that is gone; the new one's layout is awaited.
    let home = vec![Do::Warp { x: 1, y: 2000 }, Do::Ungrab, Do::Active { peer: XIPHOS, on: false }];
    assert_eq!(out, home);
    let later = now + Duration::from_secs(1);
    nothing(&p.barrier(Side::Left, 0, 10, -256, 0, later));
    p.message(XIPHOS, SESSION + 1, &Message::Layout { monitors: xiphos().wire() });
    assert_eq!(p.barrier(Side::Left, 0, 10, -256, 0, later), vec![Do::Grab]);
    // The keymap goes again on the new session.
    let out = p.grabbed(true, [0; 32], 0, now);
    let fresh = matches!(
        sent(&out)[..],
        [Message::Keymap { .. }, Message::Exit { .. }, Message::Enter { .. }]
    );
    assert!(fresh, "{out:?}");
    nothing(&p.ended(XIPHOS, SESSION));
    let out = p.ended(XIPHOS, SESSION + 1);
    let home = vec![Do::Warp { x: 1, y: 10 }, Do::Ungrab, Do::Active { peer: XIPHOS, on: false }];
    assert_eq!(out, home);
}

#[test]
fn a_grab_that_takes_after_the_session_ended_is_let_go() {
    let (mut p, now) = ready();
    p.barrier(Side::Left, 0, 10, -256, 0, now);
    p.ended(XIPHOS, SESSION);
    assert_eq!(p.grabbed(true, [0; 32], 0, now), vec![Do::Ungrab]);
    assert_eq!(p.position(), Where::Here);
}

#[test]
fn switch_crosses_at_the_middle_of_the_facing_edge_and_back() {
    let (mut p, now) = ready();
    nothing(&p.switch_to(OTHER));
    assert_eq!(p.switch_to(XIPHOS), vec![Do::Grab]);
    let out = p.grabbed(true, [0; 32], 0, now);
    let enter = Message::Enter {
        span: 1,
        side: Side::Right,
        fraction: 32768,
        modifiers: [0; 32],
        locks: 0,
    };
    assert!(sent(&out).contains(&&enter), "{out:?}");
    let out = p.switch_home();
    let exit = Do::Send { peer: XIPHOS, session: SESSION, message: Message::Exit { span: 1 } };
    assert_eq!(out[..2], [exit, Do::Warp { x: 1, y: 1440 }]);
}

#[test]
fn a_keymap_change_reaches_the_peer_at_once_while_on_it() {
    let (mut p, _) = crossed(2000);
    let (first, keys) = keymap();
    let out = p.keymap(first, keys);
    assert!(matches!(sent(&out)[..], [Message::Keymap { .. }]), "{out:?}");
}

#[test]
fn a_peer_no_longer_a_neighbour_brings_the_pointer_home() {
    let (mut p, _) = crossed(2000);
    let out = p.neighbours([None, None, None, None]);
    assert!(sent(&out).contains(&&Message::Exit { span: 1 }), "{out:?}");
    assert_eq!(out.last(), Some(&Do::Barriers(Vec::new())));
}

/// Applies the primary's messages to the secondary, as the link would.
fn deliver(s: &mut Secondary, out: &[Do<u8>]) -> Vec<Action> {
    let mut actions = Vec::new();
    for m in sent(out) {
        actions.extend(match m {
            Message::Keymap { first, keys } => {
                s.primary_keymap(*first, keys.clone());
                Vec::new()
            }
            Message::Enter { span, side, fraction, modifiers, locks } => {
                s.enter(*span, *side, *fraction, modifiers, *locks)
            }
            Message::Motion { span, dx, dy } => s.motion(*span, *dx, *dy),
            Message::Exit { span } => s.exit(*span),
            other => panic!("{other:?}"),
        });
    }
    actions
}

proptest! {
    /// Out through athena's left side at any height, a pixel into xiphos and straight back:
    /// home within a pixel of where it left.
    #[test]
    fn crossing_out_and_straight_back_returns_within_a_pixel(y in 0i32..2880) {
        let (mut p, now) = ready();
        let syms = vec![jackstay_input::Syms { level1: 0, level2: 0 }; 248];
        let mut s = Secondary::new(
            xiphos(),
            Sides::default().with(Side::Left).with(Side::Right),
            Keymap::new(8, &syms),
        );
        p.barrier(Side::Left, 0, y, -256, 0, now);
        deliver(&mut s, &p.grabbed(true, [0; 32], 0, now));
        let actions = deliver(&mut s, &p.motion(256, 0));
        let Some(Action::Leave { span, side, fraction }) = actions.last().copied() else {
            panic!("{actions:?}")
        };
        let out = p.message(XIPHOS, SESSION, &Message::Leave { span, side, fraction });
        let Some(Do::Warp { x, y: back }) = out.first().cloned() else { panic!("{out:?}") };
        prop_assert_eq!(x, 1);
        prop_assert!((back - y).abs() <= 1, "{} to {}", y, back);
    }
}

#[test]
fn a_grab_that_takes_after_the_peer_went_silent_is_let_go() {
    let (mut p, now) = ready();
    p.barrier(Side::Left, 0, 10, -256, 0, now);
    p.suspect(XIPHOS, SESSION, true);
    assert_eq!(p.grabbed(true, [0; 32], 0, now), vec![Do::Ungrab]);
    assert_eq!(p.position(), Where::Here);
}

#[test]
fn motion_too_large_to_send_is_cut_to_the_largest_that_is() {
    let (mut p, _) = crossed(2000);
    let out = p.motion(i32::MAX, i32::MIN);
    let largest = jackstay_proto::link::MAX_MOTION;
    assert_eq!(sent(&out), vec![&Message::Motion { span: 1, dx: largest, dy: -largest }]);
}

#[test]
fn a_crossing_unanswered_is_taken_back_and_not_retried_at_once() {
    let (mut p, now) = crossed(2000);
    nothing(&p.answer_due(now + ANSWER_WITHIN));
    assert_eq!(p.position(), Where::On { peer: XIPHOS, locked: false });
    let late = now + ANSWER_WITHIN + Duration::from_millis(1);
    let out = p.answer_due(late);
    let exit = Do::Send { peer: XIPHOS, session: SESSION, message: Message::Exit { span: 1 } };
    assert_eq!(
        out,
        [exit, Do::Warp { x: 1, y: 2000 }, Do::Ungrab, Do::Active { peer: XIPHOS, on: false }]
    );
    assert_eq!(p.position(), Where::Here);
    assert_eq!(p.unanswered(), Some(XIPHOS));
    // A push against the edge waits before trying the peer again; `jackstay switch` does not.
    nothing(&p.barrier(Side::Left, 0, 2000, -256, 0, late + RETRY_UNANSWERED / 2));
    let again = late + RETRY_UNANSWERED;
    assert_eq!(p.barrier(Side::Left, 0, 2000, -256, 0, again), vec![Do::Grab]);
    let (mut p, _) = crossed(2000);
    p.answer_due(late);
    assert_eq!(p.switch_to(XIPHOS), vec![Do::Grab]);
}

#[test]
fn an_answered_crossing_stays_and_clears_what_went_unanswered() {
    let (mut p, now) = crossed(2000);
    p.answer_due(now + ANSWER_WITHIN * 2);
    assert_eq!(p.unanswered(), Some(XIPHOS));
    assert_eq!(p.switch_to(XIPHOS), vec![Do::Grab]);
    p.grabbed(true, [0; 32], 0, now);
    // An answer for another span is no answer.
    nothing(&p.message(XIPHOS, SESSION, &Message::Entered { span: 1, keymap: true }));
    nothing(&p.message(XIPHOS, SESSION, &Message::Entered { span: 2, keymap: true }));
    assert_eq!(p.unanswered(), None);
    nothing(&p.answer_due(now + ANSWER_WITHIN * 10));
    assert_eq!(p.position(), Where::On { peer: XIPHOS, locked: false });
}

#[test]
fn a_peer_without_our_keymap_gets_it_again() {
    // xiphos dropped the keymap sent before the first crossing (a rival ran there): its answer
    // says so, and without the keymap again no key would reach it.
    let (mut p, _) = crossed(2000);
    let out = p.message(XIPHOS, SESSION, &Message::Entered { span: 1, keymap: false });
    let (first, keys) = keymap();
    assert_eq!(sent(&out), [&Message::Keymap { first, keys }]);
}

#[test]
fn a_secondary_answers_enter_and_says_whether_it_has_the_keymap() {
    let (mut p, now) = ready();
    let syms = vec![jackstay_input::Syms { level1: 0, level2: 0 }; 248];
    let mut s = Secondary::new(xiphos(), Sides::default(), Keymap::new(8, &syms));
    nothing(&p.switch_to(OTHER));
    p.switch_to(XIPHOS);
    let out = p.grabbed(true, [0; 32], 0, now);
    assert_eq!(deliver(&mut s, &out).last(), Some(&Action::Entered { span: 1, keymap: true }));
    let mut s = Secondary::new(xiphos(), Sides::default(), Keymap::new(8, &syms));
    assert_eq!(
        s.enter(2, Side::Left, 0, &[0; 32], 0).last(),
        Some(&Action::Entered { span: 2, keymap: false })
    );
}

#[test]
fn a_secondary_that_can_no_longer_be_driven_gives_the_pointer_back() {
    let syms = vec![jackstay_input::Syms { level1: 0, level2: 0 }; 248];
    let mut s = Secondary::new(
        xiphos(),
        Sides::default().with(Side::Left).with(Side::Right),
        Keymap::new(8, &syms),
    );
    assert_eq!(s.give_back(), []);
    s.enter(3, Side::Right, 9000, &[0; 32], 0);
    s.button(3, 1, true);
    assert_eq!(
        s.give_back(),
        [
            Action::Button { button: 1, down: false },
            Action::Leave { span: 3, side: Side::Right, fraction: 9000 },
        ]
    );
    assert!(!s.active());
    // Once it left, the primary already took the pointer back.
    s.enter(4, Side::Right, 9000, &[0; 32], 0);
    s.motion(4, 100_000 * 256, 0);
    assert_eq!(s.give_back(), []);
}
