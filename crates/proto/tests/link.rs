use jackstay_proto::link::{
    MAX_NAME_LEN, MAX_PING_INTERVAL_MS, MIN_PING_INTERVAL_MS, Message, NONCE_LEN, VERSION,
    check_decoder, decode, encode,
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
        [0, 0, 0, 13, 1, 0, 1, 0, 0, 0, 0, 0, 0, 1, 2, 2, b'a', b'b']
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
}

#[test]
fn a_frame_beyond_its_kinds_limit_is_refused_from_the_header() {
    // Each kind's limit, and one past it: the past one is refused with only 5 bytes in hand.
    for (kind, max) in [(1u8, 266u32), (2, 6), (3, 4), (4, 0), (5, 32), (6, 32), (7, 0)] {
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
    assert_eq!(decode(&[0, 0, 0, 0, 8]), Err(DecodeError::UnknownKind(8)));
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
