use jackstay_proto::link::{
    KeyEntry, LOCK_CAPS, LOCK_NUM, MAX_BUTTON, MAX_KEYMAP_KEYS, MAX_MONITORS, MAX_MOTION,
    MAX_NAME_LEN, MAX_PING_INTERVAL_MS, MIN_KEYCODE, MIN_PING_INTERVAL_MS, Message, Monitor,
    NONCE_LEN, Side, VERSION, check_decoder, decode, encode,
};
use jackstay_proto::{DecodeError, LimitError};
use proptest::prelude::*;

#[track_caller]
fn ok<T, E: std::fmt::Debug>(r: Result<T, E>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("expected Ok, got Err({e:?})"),
    }
}

fn bytes(m: &Message) -> Vec<u8> {
    let mut out = Vec::new();
    ok(encode(m, &mut out));
    out
}

#[test]
fn the_layout_is_as_documented() {
    assert_eq!(
        bytes(&Message::Hello { version: VERSION, run: 0x0102, name: "ab".into() }),
        [0, 0, 0, 13, 1, 0, 2, 0, 0, 0, 0, 0, 0, 1, 2, 2, b'a', b'b']
    );
    assert_eq!(
        bytes(&Message::Ping { seq: 7, interval_ms: 1000 }),
        [0, 0, 0, 6, 2, 0, 0, 0, 7, 3, 232]
    );
    assert_eq!(bytes(&Message::Pong { seq: 7 }), [0, 0, 0, 4, 3, 0, 0, 0, 7]);
    assert_eq!(bytes(&Message::Bye), [0, 0, 0, 0, 4]);
    let mut commit = vec![0, 0, 0, 32, 5];
    commit.extend([9; NONCE_LEN]);
    assert_eq!(bytes(&Message::Commit { digest: [9; NONCE_LEN] }), commit);
    let mut reveal = vec![0, 0, 0, 32, 6];
    reveal.extend([8; NONCE_LEN]);
    assert_eq!(bytes(&Message::Reveal { nonce: [8; NONCE_LEN] }), reveal);
    assert_eq!(bytes(&Message::Confirm), [0, 0, 0, 0, 7]);
    let monitor = Monitor { x: 939, y: 0, width: 1920, height: 1200 };
    assert_eq!(
        bytes(&Message::Layout { monitors: vec![monitor] }),
        [0, 0, 0, 9, 8, 1, 3, 171, 0, 0, 7, 128, 4, 176]
    );
    let key = KeyEntry { level1: 0x61, level2: 0x41, modifier: false };
    assert_eq!(
        bytes(&Message::Keymap { first: 38, keys: vec![key] }),
        [0, 0, 0, 11, 9, 38, 1, 0, 0, 0, 0x61, 0, 0, 0, 0x41, 0]
    );
    let mut modifiers = [0; 32];
    modifiers[6] = 4; // keycode 50
    let mut enter = vec![0, 0, 0, 40, 10, 0, 0, 0, 5, 1, 0x80, 0];
    enter.extend(modifiers);
    enter.push(LOCK_CAPS);
    assert_eq!(
        bytes(&Message::Enter {
            span: 5,
            side: Side::Right,
            fraction: 0x8000,
            modifiers,
            locks: 1
        }),
        enter
    );
    assert_eq!(
        bytes(&Message::Motion { span: 5, dx: -256, dy: 3 }),
        [0, 0, 0, 12, 11, 0, 0, 0, 5, 255, 255, 255, 0, 0, 0, 0, 3]
    );
    assert_eq!(
        bytes(&Message::Button { span: 5, button: 1, down: true }),
        [0, 0, 0, 6, 12, 0, 0, 0, 5, 1, 1]
    );
    assert_eq!(
        bytes(&Message::Key { span: 5, keycode: 66, down: false }),
        [0, 0, 0, 6, 13, 0, 0, 0, 5, 66, 0]
    );
    assert_eq!(
        bytes(&Message::Leave { span: 5, side: Side::Bottom, fraction: 7 }),
        [0, 0, 0, 7, 14, 0, 0, 0, 5, 3, 0, 7]
    );
    assert_eq!(bytes(&Message::Exit { span: 5 }), [0, 0, 0, 4, 15, 0, 0, 0, 5]);
    assert_eq!(bytes(&Message::Lock { span: 5, on: true }), [0, 0, 0, 5, 16, 0, 0, 0, 5, 1]);
}

#[test]
fn a_frame_beyond_its_kinds_limit_is_refused_from_the_header() {
    // Each kind's limit, and one past it: the past one is refused with only 5 bytes in hand.
    let kinds = [
        (1u8, 266u32),
        (2, 6),
        (3, 4),
        (4, 0),
        (5, 32),
        (6, 32),
        (7, 0),
        (8, 129),
        (9, 2234),
        (10, 40),
        (11, 12),
        (12, 6),
        (13, 6),
        (14, 7),
        (15, 4),
        (16, 5),
    ];
    for (kind, max) in kinds {
        let mut at = max.to_be_bytes().to_vec();
        at.push(kind);
        if max > 0 {
            assert_eq!(decode(&at), Ok(None), "kind {kind} at its limit is waited for");
        }
        let mut past = (max + 1).to_be_bytes().to_vec();
        past.push(kind);
        assert_eq!(decode(&past), Err(DecodeError::FrameTooLarge { len: max + 1 }), "kind {kind}");
    }
}

#[test]
fn an_unknown_kind_is_refused_from_the_header() {
    assert_eq!(decode(&[0, 0, 0, 0, 0]), Err(DecodeError::UnknownKind(0)));
    assert_eq!(decode(&[0, 0, 0, 0, 17]), Err(DecodeError::UnknownKind(17)));
    assert_eq!(decode(&[0, 0, 0, 0, 64]), Err(DecodeError::UnknownKind(64)));
}

#[test]
fn an_incomplete_frame_is_waited_for() {
    let whole = bytes(&Message::Hello { version: VERSION, run: 1, name: "athena".into() });
    for n in 0..whole.len() {
        assert_eq!(decode(&whole[..n]), Ok(None), "{n} bytes");
    }
    assert_eq!(decode(&whole), Ok(Some((decode(&whole).unwrap().unwrap().0, whole.len()))));
}

#[test]
fn bytes_after_a_message_inside_its_frame_are_refused() {
    // A Bye frame claiming 0 bytes is whole; one claiming bytes Bye has no room for is refused,
    // whether by its kind's limit or by trailing bytes.
    assert_eq!(decode(&[0, 0, 0, 1, 4, 0]), Err(DecodeError::FrameTooLarge { len: 1 }));
    // A Ping with a short payload: the fields run out.
    assert_eq!(decode(&[0, 0, 0, 5, 2, 0, 0, 0, 7, 3]), Err(DecodeError::Truncated));
    // A Hello whose name is shorter than its length byte says.
    assert_eq!(
        decode(&[0, 0, 0, 12, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 2, b'a']),
        Err(DecodeError::Truncated)
    );
    // A Hello whose name is longer than its length byte says.
    assert_eq!(
        decode(&[0, 0, 0, 13, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1, b'a', b'b']),
        Err(DecodeError::TrailingBytes)
    );
}

#[test]
fn the_longest_name_is_kept_and_a_longer_one_refused() {
    let longest = "é".repeat(MAX_NAME_LEN / 2) + "x";
    assert_eq!(longest.len(), MAX_NAME_LEN);
    let m = Message::Hello { version: VERSION, run: 3, name: longest };
    let b = bytes(&m);
    assert_eq!(b.len(), 5 + 266);
    assert_eq!(decode(&b), Ok(Some((m, b.len()))));
    let mut out = Vec::new();
    let longer = Message::Hello { version: VERSION, run: 3, name: "x".repeat(MAX_NAME_LEN + 1) };
    assert_eq!(encode(&longer, &mut out), Err(LimitError::TextTooLong { len: MAX_NAME_LEN + 1 }));
    assert_eq!(out, Vec::<u8>::new());
}

#[test]
fn a_name_that_is_not_utf8_is_refused() {
    assert_eq!(
        decode(&[0, 0, 0, 12, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0xff]),
        Err(DecodeError::Limit(LimitError::TextNotUtf8))
    );
}

#[test]
fn a_name_is_carried_as_sent_even_when_unprintable() {
    // Display sanitizes; the wire does not, so what a user sees can be traced to what was sent.
    let m = Message::Hello { version: VERSION, run: 3, name: "a\u{1b}[2Jb\u{202e}".into() };
    let b = bytes(&m);
    assert_eq!(decode(&b), Ok(Some((m, b.len()))));
}

#[test]
fn ping_intervals_are_limited_both_ways() {
    for interval_ms in [MIN_PING_INTERVAL_MS, 100, 1000, MAX_PING_INTERVAL_MS] {
        let m = Message::Ping { seq: 1, interval_ms };
        let b = bytes(&m);
        assert_eq!(decode(&b), Ok(Some((m, b.len()))));
    }
    for interval_ms in [0, MIN_PING_INTERVAL_MS - 1, MAX_PING_INTERVAL_MS + 1, u16::MAX] {
        let mut out = Vec::new();
        let refused = Err(LimitError::BadInterval { interval_ms });
        assert_eq!(encode(&Message::Ping { seq: 1, interval_ms }, &mut out), refused);
        assert_eq!(out, Vec::<u8>::new());
        let [hi, lo] = interval_ms.to_be_bytes();
        assert_eq!(
            decode(&[0, 0, 0, 6, 2, 0, 0, 0, 1, hi, lo]),
            Err(DecodeError::Limit(LimitError::BadInterval { interval_ms }))
        );
    }
}

#[test]
fn messages_follow_one_another_in_a_buffer() {
    let mut buf = bytes(&Message::Ping { seq: 1, interval_ms: 100 });
    buf.extend(bytes(&Message::Pong { seq: 2 }));
    let (first, used) = ok(decode(&buf)).unwrap();
    assert_eq!(first, Message::Ping { seq: 1, interval_ms: 100 });
    assert_eq!(ok(decode(&buf[used..])), Some((Message::Pong { seq: 2 }, buf.len() - used)));
}

/// Encoding `m` is refused with `e`, and its frame (built by hand as `frame`) is refused by the
/// decoder the same way.
#[track_caller]
fn refused_both_ways(m: &Message, frame: &[u8], e: &DecodeError) {
    let mut out = Vec::new();
    match e {
        DecodeError::Limit(limit) => assert_eq!(encode(m, &mut out), Err(limit.clone())),
        _ => panic!("{e:?} is not a limit"),
    }
    assert_eq!(out, Vec::<u8>::new());
    assert_eq!(decode(frame).as_ref(), Err(e));
}

fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = u32::try_from(payload.len()).unwrap().to_be_bytes().to_vec();
    out.push(kind);
    out.extend(payload);
    out
}

#[test]
fn layouts_are_limited_both_ways() {
    let m = Monitor { x: 0, y: 0, width: 10, height: 10 };
    let many = Message::Layout { monitors: vec![m; MAX_MONITORS] };
    assert_eq!(decode(&bytes(&many)).map(|d| d.map(|(m, _)| m)), Ok(Some(many)));
    let limit = |count| DecodeError::Limit(LimitError::TooManyEntries { count });
    refused_both_ways(&Message::Layout { monitors: vec![] }, &frame(8, &[0]), &limit(0));
    let mut seventeen = vec![17];
    seventeen.extend([0, 0, 0, 0, 0, 10, 0, 10].repeat(16));
    // The count is refused before the monitors are read; 16 fit the kind's limit.
    assert_eq!(decode(&frame(8, &seventeen)), Err(limit(17)));
    let bad = DecodeError::Limit(LimitError::BadMonitor);
    for (x, width) in [(0u16, 0u16), (32_767, 2), (32_760, 9), (2, 32_767)] {
        let mut p = vec![1];
        for v in [x, 0, width, 10] {
            p.extend(v.to_be_bytes());
        }
        let monitors = vec![Monitor { x, y: 0, width, height: 10 }];
        refused_both_ways(&Message::Layout { monitors }, &frame(8, &p), &bad);
    }
    // The largest: a monitor whose last pixel is at x 32767.
    let edge = Message::Layout { monitors: vec![Monitor { x: 32_767, y: 0, width: 1, height: 1 }] };
    assert!(decode(&bytes(&edge)).is_ok_and(|d| d.is_some()));
}

#[test]
fn keymaps_are_limited_both_ways() {
    let key = KeyEntry { level1: 1, level2: 2, modifier: true };
    let all = Message::Keymap { first: MIN_KEYCODE, keys: vec![key; MAX_KEYMAP_KEYS] };
    assert_eq!(bytes(&all).len(), 5 + 2234);
    assert_eq!(decode(&bytes(&all)).map(|d| d.map(|(m, _)| m)), Ok(Some(all)));
    let entry = [0, 0, 0, 1, 0, 0, 0, 2, 1];
    for (first, count) in [(7u8, 1usize), (8, 0), (255, 2), (9, MAX_KEYMAP_KEYS)] {
        let mut p = vec![first, u8::try_from(count).unwrap()];
        p.extend(entry.repeat(count));
        let e = DecodeError::Limit(LimitError::TooManyEntries { count });
        refused_both_ways(&Message::Keymap { first, keys: vec![key; count] }, &frame(9, &p), &e);
    }
    // A modifier flag that is neither 0 nor 1.
    assert_eq!(decode(&frame(9, &[8, 1, 0, 0, 0, 1, 0, 0, 0, 2, 2])), Err(DecodeError::BadFlag(2)));
}

#[test]
fn input_messages_are_limited_both_ways() {
    let mut low = [0u8; 32];
    low[0] = 1; // keycode 0
    let mut p = vec![0, 0, 0, 1, 0, 0, 0];
    p.extend(low);
    p.push(0);
    let enter = |modifiers, locks| Message::Enter {
        span: 1,
        side: Side::Left,
        fraction: 0,
        modifiers,
        locks,
    };
    refused_both_ways(
        &enter(low, 0),
        &frame(10, &p),
        &DecodeError::Limit(LimitError::UnknownCode { code: 0 }),
    );
    let mut p = vec![0, 0, 0, 1, 0, 0, 0];
    p.extend([0; 32]);
    p.push(4);
    refused_both_ways(
        &enter([0; 32], 4),
        &frame(10, &p),
        &DecodeError::Limit(LimitError::UnknownCode { code: 4 }),
    );
    assert!(decode(&bytes(&enter([0; 32], LOCK_CAPS | LOCK_NUM))).is_ok_and(|d| d.is_some()));
    // A side out of range.
    assert_eq!(
        decode(&frame(14, &[0, 0, 0, 1, 4, 0, 0])),
        Err(DecodeError::Limit(LimitError::UnknownCode { code: 4 }))
    );

    for d in [MAX_MOTION + 1, -MAX_MOTION - 1, i32::MIN, i32::MAX] {
        let mut p = vec![0, 0, 0, 1];
        p.extend(d.to_be_bytes());
        p.extend(0i32.to_be_bytes());
        let e = DecodeError::Limit(LimitError::BadMotion);
        refused_both_ways(&Message::Motion { span: 1, dx: d, dy: 0 }, &frame(11, &p), &e);
    }
    for d in [MAX_MOTION, -MAX_MOTION] {
        assert!(
            decode(&bytes(&Message::Motion { span: 1, dx: 0, dy: d })).is_ok_and(|d| d.is_some())
        );
    }

    for button in [0, MAX_BUTTON + 1, u8::MAX] {
        let e = DecodeError::Limit(LimitError::UnknownCode { code: button });
        let m = Message::Button { span: 1, button, down: true };
        refused_both_ways(&m, &frame(12, &[0, 0, 0, 1, button, 1]), &e);
    }
    for keycode in [0, MIN_KEYCODE - 1] {
        let e = DecodeError::Limit(LimitError::UnknownCode { code: keycode });
        let m = Message::Key { span: 1, keycode, down: true };
        refused_both_ways(&m, &frame(13, &[0, 0, 0, 1, keycode, 1]), &e);
    }
    // Up or down is 0 or 1, never anything else: each frame has one encoding.
    assert_eq!(decode(&frame(12, &[0, 0, 0, 1, 1, 2])), Err(DecodeError::BadFlag(2)));
    assert_eq!(decode(&frame(13, &[0, 0, 0, 1, 66, 255])), Err(DecodeError::BadFlag(255)));
    assert_eq!(decode(&frame(16, &[0, 0, 0, 1, 2])), Err(DecodeError::BadFlag(2)));
}

fn arb_side() -> impl Strategy<Value = Side> {
    prop_oneof![Just(Side::Left), Just(Side::Right), Just(Side::Top), Just(Side::Bottom)]
}

fn arb_monitor() -> impl Strategy<Value = Monitor> {
    (0u16..32_000, 0u16..32_000, 1u16..=767, 1u16..=767).prop_map(|(x, y, width, height)| Monitor {
        x,
        y,
        width,
        height,
    })
}

fn arb_input() -> impl Strategy<Value = Message> {
    prop_oneof![
        proptest::collection::vec(arb_monitor(), 1..=MAX_MONITORS)
            .prop_map(|monitors| Message::Layout { monitors }),
        (
            MIN_KEYCODE..=u8::MAX,
            proptest::collection::vec(
                (any::<u32>(), any::<u32>(), any::<bool>()),
                1..=MAX_KEYMAP_KEYS
            )
        )
            .prop_filter("fits", |(first, keys)| usize::from(*first) + keys.len() <= 256)
            .prop_map(|(first, keys)| Message::Keymap {
                first,
                keys: keys
                    .into_iter()
                    .map(|(level1, level2, modifier)| KeyEntry { level1, level2, modifier })
                    .collect(),
            }),
        (any::<u32>(), arb_side(), any::<u16>(), any::<[u8; 32]>(), 0u8..4).prop_map(
            |(span, side, fraction, mut modifiers, locks)| {
                modifiers[0] = 0;
                Message::Enter { span, side, fraction, modifiers, locks }
            }
        ),
        (any::<u32>(), -MAX_MOTION..=MAX_MOTION, -MAX_MOTION..=MAX_MOTION)
            .prop_map(|(span, dx, dy)| Message::Motion { span, dx, dy }),
        (any::<u32>(), 1..=MAX_BUTTON, any::<bool>())
            .prop_map(|(span, button, down)| Message::Button { span, button, down }),
        (any::<u32>(), MIN_KEYCODE..=u8::MAX, any::<bool>())
            .prop_map(|(span, keycode, down)| Message::Key { span, keycode, down }),
        (any::<u32>(), arb_side(), any::<u16>())
            .prop_map(|(span, side, fraction)| Message::Leave { span, side, fraction }),
        any::<u32>().prop_map(|span| Message::Exit { span }),
        (any::<u32>(), any::<bool>()).prop_map(|(span, on)| Message::Lock { span, on }),
    ]
}

fn arb_message() -> impl Strategy<Value = Message> {
    prop_oneof![
        (any::<u16>(), any::<u64>(), "\\PC{0,60}")
            .prop_filter("fits", |(_, _, n)| n.len() <= MAX_NAME_LEN)
            .prop_map(|(version, run, name)| Message::Hello { version, run, name }),
        (any::<u32>(), MIN_PING_INTERVAL_MS..=MAX_PING_INTERVAL_MS)
            .prop_map(|(seq, interval_ms)| Message::Ping { seq, interval_ms }),
        any::<u32>().prop_map(|seq| Message::Pong { seq }),
        Just(Message::Bye),
        any::<[u8; NONCE_LEN]>().prop_map(|digest| Message::Commit { digest }),
        any::<[u8; NONCE_LEN]>().prop_map(|nonce| Message::Reveal { nonce }),
        Just(Message::Confirm),
        arb_input(),
    ]
}

proptest! {
    #[test]
    fn messages_round_trip(m in arb_message()) {
        let b = bytes(&m);
        prop_assert_eq!(decode(&b), Ok(Some((m, b.len()))));
    }

    #[test]
    fn any_bytes_hold_the_decoders_properties(data in proptest::collection::vec(any::<u8>(), 0..300)) {
        prop_assert_eq!(check_decoder(&data), Ok(()));
    }

    #[test]
    fn any_valid_message_with_any_tail_holds_the_decoders_properties(
        m in arb_message(),
        tail in proptest::collection::vec(any::<u8>(), 0..20),
    ) {
        let mut data = bytes(&m);
        data.extend(tail);
        prop_assert_eq!(check_decoder(&data), Ok(()));
    }
}
