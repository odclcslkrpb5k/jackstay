use jackstay_input::{Action, Keymap, Layout, Pointer, Secondary, Sides, Step, Syms};
use jackstay_proto::link::{KeyEntry, LOCK_CAPS, Monitor, Side};
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

fn both() -> Sides {
    Sides::default().with(Side::Left).with(Side::Right)
}

#[test]
fn the_owners_edges_are_the_monitors_on_each_side() {
    let x = xiphos();
    assert_eq!(x.edge(Side::Left).spans, vec![(1200, 4080)]);
    assert_eq!(x.edge(Side::Right).spans, vec![(1200, 4080)]);
    assert_eq!(x.edge(Side::Right).across, 3439);
    assert_eq!(x.edge(Side::Top).spans, vec![(939, 2859)]);
    assert_eq!(athena().edge(Side::Left).spans, vec![(0, 2880)]);
    assert_eq!(athena().edge(Side::Right).across, 5119);
}

#[test]
fn athenas_sides_meet_xiphos_one_to_one_below_its_dead_zones() {
    let (a, x) = (athena(), xiphos());
    for y in [0, 1, 719, 1439, 1440, 2000, 2879] {
        let f = a.edge(Side::Left).fraction(y);
        assert_eq!(x.edge(Side::Right).point(f), (3439, y + 1200), "athena's left at y {y}");
        let f = a.edge(Side::Right).fraction(y);
        assert_eq!(x.edge(Side::Left).point(f), (0, y + 1200), "athena's right at y {y}");
        // And back.
        let f = x.edge(Side::Left).fraction(y + 1200);
        assert_eq!(a.edge(Side::Right).point(f), (5119, y));
    }
}

#[test]
fn an_edge_with_a_gap_skips_it() {
    // Two monitors on the left side with a 100-pixel gap between them.
    let l = layout(&[(0, 0, 100, 100), (0, 200, 100, 100)]);
    let e = l.edge(Side::Left);
    assert_eq!(e.covered(), 200);
    assert_eq!(e.along(e.fraction(99)), 99);
    assert_eq!(e.along(e.fraction(200)), 200);
    // A point in the gap counts as the next covered pixel.
    assert_eq!(e.along(e.fraction(150)), 200);
    // Half way along the covered length is the second monitor's first pixel.
    assert_eq!(e.along(1 << 15), 200);
}

#[test]
fn a_short_edge_meeting_a_long_one_maps_by_proportion() {
    let short = layout(&[(0, 0, 100, 1440)]).edge(Side::Right);
    let long = layout(&[(0, 0, 100, 2880)]).edge(Side::Left);
    assert_eq!(long.along(short.fraction(0)), 0);
    assert_eq!(long.along(short.fraction(720)), 1440);
    assert_eq!(long.along(short.fraction(1439)), 2878);
}

#[test]
fn the_pointer_never_enters_xiphos_dead_zones() {
    let x = xiphos();
    // On DP-1-1 under the left dead zone, pushing up: it stops at the top of DP-1-1.
    let mut p = Pointer::at(&x, 100, 1210);
    assert_eq!(p.move_by(&x, 0, -50 * 256, both(), true), Step::At { x: 100, y: 1200 });
    // Under HDMI-2, pushing up: it moves onto HDMI-2.
    let mut p = Pointer::at(&x, 1000, 1210);
    assert_eq!(p.move_by(&x, 0, -50 * 256, both(), true), Step::At { x: 1000, y: 1160 });
    // On HDMI-2, pushing left past its edge at x 939: that side is not xiphos's left side, so
    // it stops, and never leaves.
    let mut p = Pointer::at(&x, 945, 500);
    assert_eq!(p.move_by(&x, -50 * 256, 0, both(), true), Step::At { x: 939, y: 500 });
}

#[test]
fn the_pointer_leaves_only_through_a_side_with_a_neighbour() {
    let x = xiphos();
    let mut p = Pointer::at(&x, 2, 2000);
    let fraction = x.edge(Side::Left).fraction(2000);
    assert_eq!(
        p.move_by(&x, -10 * 256, 0, both(), true),
        Step::Left { side: Side::Left, fraction }
    );
    let mut p = Pointer::at(&x, 2, 2000);
    let only_right = Sides::default().with(Side::Right);
    assert_eq!(p.move_by(&x, -10 * 256, 0, only_right, true), Step::At { x: 0, y: 2000 });
    // Not with a button held: a drag stays a drag.
    let mut p = Pointer::at(&x, 2, 2000);
    assert_eq!(p.move_by(&x, -10 * 256, 0, both(), false), Step::At { x: 0, y: 2000 });
    // Not through the bottom, which has no neighbour.
    let mut p = Pointer::at(&x, 100, 4078);
    assert_eq!(p.move_by(&x, 0, 10 * 256, both(), true), Step::At { x: 100, y: 4079 });
}

#[test]
fn fractions_of_a_pixel_are_carried() {
    let a = athena();
    let mut p = Pointer::at(&a, 100, 100);
    // Ten moves of 0.4 px: 4 px, where rounding each would have given none.
    for _ in 0..10 {
        let _ = p.move_by(&a, 102, 0, Sides::default(), true);
    }
    assert_eq!(p.pixel(), (104, 100));
}

#[test]
fn the_pointer_follows_a_layout_change() {
    let mut p = Pointer::at(&athena(), 4000, 2000);
    let smaller = layout(&[(0, 0, 2560, 1440)]);
    p.relayout(&smaller);
    assert_eq!(p.pixel(), (2559, 1439));
}

fn keymap(entries: &[(u8, u32, u32)]) -> Keymap {
    let mut syms = vec![Syms::default(); 248];
    for &(k, level1, level2) in entries {
        syms[usize::from(k - 8)] = Syms { level1, level2 };
    }
    Keymap::new(8, &syms)
}

fn primary_keymap(entries: &[(u8, u32, u32, bool)]) -> Vec<KeyEntry> {
    let mut keys = vec![KeyEntry::default(); 248];
    for &(k, level1, level2, modifier) in entries {
        keys[usize::from(k - 8)] = KeyEntry { level1, level2, modifier };
    }
    keys
}

const A: u32 = 0x61;
const SHIFT_L: u32 = 0xffe1;
const CAPS_LOCK: u32 = 0xffe5;
const SUPER_L: u32 = 0xffeb;

/// xiphos's keys: `a` on 38, `Shift_L` on 50, `Caps_Lock` on 66, `Super_L` on 133.
fn xiphos_secondary() -> Secondary {
    let here = keymap(&[(38, A, 0x41), (50, SHIFT_L, 0), (66, CAPS_LOCK, 0), (133, SUPER_L, 0)]);
    let mut s = Secondary::new(xiphos(), both(), here);
    // athena: caps:super makes 66 Super_L too.
    s.primary_keymap(
        8,
        primary_keymap(&[
            (38, A, 0x41, false),
            (50, SHIFT_L, 0, true),
            (66, SUPER_L, 0, true),
            (133, SUPER_L, 0, true),
        ]),
    );
    s
}

fn bitmap(keys: &[u8]) -> [u8; 32] {
    let mut b = [0u8; 32];
    for k in keys {
        b[usize::from(k / 8)] |= 1 << (k % 8);
    }
    b
}

#[test]
fn athenas_caps_lock_is_xiphos_super() {
    let mut s = xiphos_secondary();
    let _ = s.enter(1, Side::Right, 0, &[0; 32], 0);
    assert_eq!(s.key(1, 66, true), vec![Action::Key { keycode: 133, down: true }]);
    // athena's own Super, pressed too: the key here is already down.
    assert_eq!(s.key(1, 133, true), vec![]);
    // Up only when both are.
    assert_eq!(s.key(1, 66, false), vec![]);
    assert_eq!(s.key(1, 133, false), vec![Action::Key { keycode: 133, down: false }]);
}

#[test]
fn a_key_is_released_as_the_key_it_was_pressed_as() {
    let mut s = xiphos_secondary();
    let _ = s.enter(1, Side::Right, 0, &[0; 32], 0);
    assert_eq!(s.key(1, 66, true), vec![Action::Key { keycode: 133, down: true }]);
    // xiphos's keymap changes while it is held: Super_L moves to 134.
    s.keymap(keymap(&[(38, A, 0x41), (134, SUPER_L, 0)]));
    assert_eq!(s.key(1, 66, false), vec![Action::Key { keycode: 133, down: false }]);
}

#[test]
fn enter_presses_only_the_held_modifiers_and_sets_the_locks() {
    let mut s = xiphos_secondary();
    // Shift and the letter a held on athena as the pointer crosses: only Shift is pressed.
    let out = s.enter(1, Side::Right, 0, &bitmap(&[50, 38]), LOCK_CAPS);
    assert_eq!(
        out,
        vec![
            Action::MoveTo { x: 3439, y: 1200 },
            Action::Locks { caps: true, num: false },
            Action::Key { keycode: 50, down: true },
        ]
    );
    // Its release on athena, forwarded, releases it here.
    assert_eq!(s.key(1, 50, false), vec![Action::Key { keycode: 50, down: false }]);
}

/// Every way a span can end, from a span holding `a`, Shift and button 1.
type End = fn(&mut Secondary) -> Vec<Action>;

fn ends() -> Vec<(&'static str, End)> {
    vec![
        ("exit", |s| s.exit(1)),
        ("end (suspect, closed, replaced)", Secondary::end),
        ("a new enter", |s| s.enter(2, Side::Left, 0, &[0; 32], 0)),
    ]
}

#[test]
fn every_way_a_span_ends_releases_everything_held() {
    for (how, end) in ends() {
        let mut s = xiphos_secondary();
        let _ = s.enter(1, Side::Right, 0, &[0; 32], 0);
        let _ = s.key(1, 50, true);
        let _ = s.key(1, 38, true);
        let _ = s.button(1, 1, true);
        let out = end(&mut s);
        for released in [
            Action::Button { button: 1, down: false },
            Action::Key { keycode: 38, down: false },
            Action::Key { keycode: 50, down: false },
        ] {
            assert!(out.contains(&released), "{how}: {out:?}");
        }
    }
}

#[test]
fn leaving_releases_everything_and_ends_the_span() {
    let mut s = xiphos_secondary();
    let _ = s.enter(1, Side::Right, s_fraction(2000), &[0; 32], 0);
    let _ = s.key(1, 50, true);
    let out = s.motion(1, 10 * 256, 0);
    assert_eq!(
        out,
        vec![
            Action::Key { keycode: 50, down: false },
            Action::Leave { span: 1, side: Side::Right, fraction: s_fraction(2000) },
        ]
    );
    assert!(!s.active());
    // The rest of the span is dropped: a late key press is never injected.
    assert_eq!(s.key(1, 38, true), vec![]);
    assert_eq!(s.motion(1, -256, 0), vec![]);
}

fn s_fraction(y: i32) -> u16 {
    xiphos().edge(Side::Right).fraction(y)
}

#[test]
fn a_stale_spans_messages_change_nothing() {
    let mut s = xiphos_secondary();
    let _ = s.enter(7, Side::Right, 0, &[0; 32], 0);
    assert_eq!(s.key(6, 38, true), vec![]);
    assert_eq!(s.button(6, 1, true), vec![]);
    assert_eq!(s.motion(6, -256, 0), vec![]);
    assert_eq!(s.exit(6), vec![]);
    assert!(s.active());
}

#[test]
fn no_leave_while_a_button_is_held() {
    let mut s = xiphos_secondary();
    let _ = s.enter(1, Side::Right, s_fraction(2000), &[0; 32], 0);
    let _ = s.button(1, 1, true);
    assert_eq!(s.motion(1, 10 * 256, 0), vec![]);
    assert!(s.active());
    let _ = s.button(1, 1, false);
    assert!(matches!(s.motion(1, 10 * 256, 0).last(), Some(Action::Leave { .. })));
}

fn arb_layout() -> impl Strategy<Value = Layout> {
    proptest::collection::vec((0u16..6000, 0u16..6000, 1u16..4000, 1u16..4000), 1..=6).prop_map(
        |ms| {
            let monitors: Vec<Monitor> = ms
                .into_iter()
                .map(|(x, y, width, height)| Monitor { x, y, width, height })
                .collect();
            Layout::new(&monitors).unwrap_or_else(|e| panic!("{e:?}"))
        },
    )
}

fn arb_side() -> impl Strategy<Value = Side> {
    prop_oneof![Just(Side::Left), Just(Side::Right), Just(Side::Top), Just(Side::Bottom)]
}

proptest! {
    #[test]
    fn every_covered_pixel_survives_a_round_trip(l in arb_layout(), side in arb_side()) {
        let e = l.edge(side);
        for &(start, end) in &e.spans {
            for along in (start..end).step_by(7).chain([end - 1]) {
                prop_assert_eq!(e.along(e.fraction(along)), along);
            }
        }
    }

    #[test]
    fn an_entry_point_is_always_on_a_monitor(l in arb_layout(), side in arb_side(), f in any::<u16>()) {
        let (x, y) = l.edge(side).point(f);
        prop_assert!(l.monitor_at(x, y).is_some(), "({x}, {y})");
    }

    #[test]
    fn the_pointer_stays_on_a_monitor(
        l in arb_layout(),
        start in (0i32..10000, 0i32..10000),
        moves in proptest::collection::vec((-100_000i32..100_000, -100_000i32..100_000), 1..50),
    ) {
        let mut p = Pointer::at(&l, start.0, start.1);
        for (dx, dy) in moves {
            if let Step::At { x, y } = p.move_by(&l, dx, dy, Sides::default(), true) {
                prop_assert!(l.monitor_at(x, y).is_some(), "({x}, {y})");
                prop_assert_eq!(p.pixel(), (x, y));
            }
        }
    }

    #[test]
    fn whatever_the_primary_sends_ends_with_nothing_held(
        ops in proptest::collection::vec((0u8..4, 1u32..4, 8u8..=255, any::<bool>(), -5000i32..5000), 0..60),
    ) {
        let mut s = xiphos_secondary();
        let mut down = std::collections::BTreeSet::new();
        let track = |actions: Vec<Action>, down: &mut std::collections::BTreeSet<(bool, u8)>| {
            for a in actions {
                match a {
                    Action::Key { keycode, down: d } => { if d { down.insert((true, keycode)); } else { down.remove(&(true, keycode)); } }
                    Action::Button { button, down: d } => { if d { down.insert((false, button)); } else { down.remove(&(false, button)); } }
                    _ => {}
                }
            }
        };
        for (op, span, code, d, delta) in ops {
            let actions = match op {
                0 => s.enter(span, Side::Right, 0, &bitmap(&[50]), 0),
                1 => s.key(span, code, d),
                2 => s.button(span, code % 9 + 1, d),
                _ => s.motion(span, delta * 256, delta),
            };
            track(actions, &mut down);
        }
        let end = s.end();
        track(end, &mut down);
        prop_assert!(down.is_empty(), "still held: {down:?}");
    }
}

// Review of slice 1, 2026-10-29: each test below failed before its fix.

#[test]
fn locked_to_the_secondary_the_pointer_never_leaves_it() {
    let mut s = xiphos_secondary();
    let _ = s.enter(1, Side::Right, s_fraction(2000), &[0; 32], 0);
    s.lock(1, true);
    assert_eq!(s.motion(1, 10 * 256, 0), vec![]);
    assert!(s.active());
    assert_eq!(s.key(1, 38, true), vec![Action::Key { keycode: 38, down: true }]);
    s.lock(1, false);
    assert!(matches!(s.motion(1, 10 * 256, 0).last(), Some(Action::Leave { .. })));
}

#[test]
fn after_silence_the_backlog_is_dropped_until_the_primarys_exit() {
    let mut s = xiphos_secondary();
    // Silence, then QUIC delivers what the primary sent meanwhile: a crossing and a key.
    let _ = s.suspect();
    assert_eq!(s.enter(5, Side::Right, 0, &[0; 32], 0), vec![]);
    assert_eq!(s.key(5, 38, true), vec![]);
    assert!(!s.active());
    // The primary's Exit, sent when it noticed the silence, ends the backlog.
    assert_eq!(s.exit(5), vec![]);
    assert!(matches!(s.enter(6, Side::Right, 0, &[0; 32], 0).first(), Some(Action::MoveTo { .. })));
    assert_eq!(s.key(6, 38, true), vec![Action::Key { keycode: 38, down: true }]);
}

#[test]
fn a_new_session_has_no_backlog() {
    let mut s = xiphos_secondary();
    let _ = s.suspect();
    let _ = s.reset();
    assert!(matches!(s.enter(1, Side::Right, 0, &[0; 32], 0).first(), Some(Action::MoveTo { .. })));
}

#[test]
fn no_key_is_injected_before_the_primarys_keymap() {
    let here = keymap(&[(38, A, 0x41), (66, CAPS_LOCK, 0), (133, SUPER_L, 0)]);
    let mut s = Secondary::new(xiphos(), both(), here);
    let _ = s.enter(1, Side::Right, 0, &[0; 32], 0);
    // athena's 66 is Super_L; untranslated it would be xiphos's Caps Lock.
    assert_eq!(s.key(1, 66, true), vec![]);
}

#[test]
fn a_key_with_no_symbols_on_the_primary_is_dropped() {
    let here = keymap(&[(38, A, 0x41)]);
    let mut s = Secondary::new(xiphos(), both(), here);
    s.primary_keymap(8, primary_keymap(&[]));
    let _ = s.enter(1, Side::Right, 0, &[0; 32], 0);
    assert_eq!(s.key(1, 38, true), vec![]);
}

#[test]
fn a_long_move_leaves_through_the_monitor_it_reaches() {
    let a = athena();
    let right = Sides::default().with(Side::Right);
    let mut p = Pointer::at(&a, 2550, 100);
    let fraction = a.edge(Side::Right).fraction(100);
    assert_eq!(
        p.move_by(&a, 3000 * 256, 0, right, true),
        Step::Left { side: Side::Right, fraction }
    );
    // Mirrored monitors of two sizes: the larger one's edge is the side.
    let m = layout(&[(0, 0, 1920, 1080), (0, 0, 2560, 1440)]);
    let mut p = Pointer::at(&m, 1900, 100);
    let fraction = m.edge(Side::Right).fraction(100);
    assert_eq!(
        p.move_by(&m, 700 * 256, 0, right, true),
        Step::Left { side: Side::Right, fraction }
    );
}

#[test]
fn the_secondarys_own_mouse_moves_the_pointer_the_primary_drives() {
    let mut s = xiphos_secondary();
    s.enter(1, Side::Right, 0, &[0; 32], 0);
    s.moved(100, 1300);
    assert_eq!(s.motion(1, 256, 0), vec![Action::MoveTo { x: 101, y: 1300 }]);
    // Off every monitor (the dead zone above-left): the nearest pixel on one, HDMI-2's.
    s.moved(10, 10);
    assert_eq!(s.motion(1, 0, 256), vec![Action::MoveTo { x: 939, y: 11 }]);
}
