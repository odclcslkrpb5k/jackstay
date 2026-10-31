//! Clipboard sharing's link messages (docs/sharing.md, "Messages"): layouts, limits both ways,
//! and the decoder's properties over them.

use jackstay_proto::link::{Message, check_decoder, decode, encode};
use jackstay_proto::sharing::{
    Announce, MAX_ANNOUNCE_LEN, MAX_FORGETS, MAX_INLINE_LEN, MAX_INLINE_TOTAL, MAX_NAME_LEN,
    MAX_TARGETS, Name, Target,
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

fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = ok(u32::try_from(payload.len())).to_be_bytes().to_vec();
    out.push(kind);
    out.extend(payload);
    out
}

fn name(s: &str) -> Name {
    ok(Name::new(s.as_bytes()))
}

fn text(inline: Option<&[u8]>, size: u64) -> Target {
    Target {
        name: name("UTF8_STRING"),
        type_name: name("UTF8_STRING"),
        format: 8,
        size,
        sha256: [7; 32],
        inline: inline.map(<[u8]>::to_vec),
    }
}

fn announce(targets: Vec<Target>) -> Message {
    Message::Announce(Box::new(Announce {
        tick: 5,
        digest: [1; 32],
        replaces: None,
        copied_at_ms: 9,
        targets,
    }))
}

/// Refused when built, and refused when decoded from `frame`.
#[track_caller]
fn refused_both_ways(m: &Message, frame: &[u8], e: &LimitError) {
    let mut out = Vec::new();
    assert_eq!(encode(m, &mut out), Err(e.clone()));
    assert_eq!(out, Vec::<u8>::new());
    assert_eq!(decode(frame), Err(DecodeError::Limit(e.clone())));
}

#[test]
fn the_layout_is_as_documented() {
    let mut want = vec![0, 0, 0, 120, 18];
    want.extend([0, 0, 0, 0, 0, 0, 0, 5]);
    want.extend([1; 32]);
    want.push(0);
    want.extend([0, 0, 0, 0, 0, 0, 0, 9]);
    want.push(1);
    for n in [b"UTF8_STRING", b"UTF8_STRING"] {
        want.extend([0, 11]);
        want.extend(n);
    }
    want.push(8);
    want.extend([0, 0, 0, 0, 0, 0, 0, 2]);
    want.extend([7; 32]);
    want.extend([1, b'h', b'i']);
    assert_eq!(bytes(&announce(vec![text(Some(b"hi"), 2)])), want);
    assert_eq!(bytes(&Message::Emptied { tick: 3 }), [0, 0, 0, 8, 19, 0, 0, 0, 0, 0, 0, 0, 3]);
    let mut forget = vec![0, 0, 0, 42, 20, 0, 0, 0, 0, 0, 0, 0, 3, 0, 1];
    forget.extend([2; 32]);
    assert_eq!(bytes(&Message::Forget { tick: 3, digests: vec![[2; 32]] }), forget);
    let mut fetch = vec![0, 0, 0, 43, 21];
    fetch.extend([2; 32]);
    fetch.extend([0, 9]);
    fetch.extend(b"image/png");
    assert_eq!(bytes(&Message::Fetch { digest: [2; 32], name: name("image/png") }), fetch);
    assert_eq!(bytes(&Message::Answer { held: true }), [0, 0, 0, 1, 22, 1]);
}

#[test]
fn each_kind_is_refused_past_its_limit_from_the_header() {
    for (kind, max) in [(18u8, MAX_ANNOUNCE_LEN), (19, 8), (20, 32_010), (21, 4130), (22, 1)] {
        let mut at = max.to_be_bytes().to_vec();
        at.push(kind);
        assert_eq!(decode(&at), Ok(None), "kind {kind} at its limit is waited for");
        let mut past = (max + 1).to_be_bytes().to_vec();
        past.push(kind);
        assert_eq!(decode(&past), Err(DecodeError::FrameTooLarge { len: max + 1 }), "kind {kind}");
    }
}

#[test]
fn names_are_latin1_without_nul_and_bounded() {
    assert_eq!(Name::new(b""), Err(LimitError::BadName));
    assert_eq!(Name::new(b"a\0b"), Err(LimitError::BadName));
    assert_eq!(Name::new(&vec![b'x'; MAX_NAME_LEN + 1]), Err(LimitError::BadName));
    // Any other byte is a Latin-1 character, spaces and quotes included.
    let longest = vec![0xe9; MAX_NAME_LEN];
    assert_eq!(ok(Name::new(&longest)).latin1(), longest.as_slice());
    let m = Message::Fetch { digest: [0; 32], name: ok(Name::new(b"text/x \"odd\" \xff")) };
    let b = bytes(&m);
    assert_eq!(decode(&b), Ok(Some((m, b.len()))));
    // Decoded names keep the same rules: empty, and holding NUL.
    let mut empty = vec![0; 32];
    empty.extend([0, 0]);
    assert_eq!(decode(&frame(21, &empty)), Err(DecodeError::Limit(LimitError::BadName)));
    let mut nul = vec![0; 32];
    nul.extend([0, 1, 0]);
    assert_eq!(decode(&frame(21, &nul)), Err(DecodeError::Limit(LimitError::BadName)));
}

fn targets_of(m: &Message) -> Vec<Target> {
    match m {
        Message::Announce(a) => a.targets.clone(),
        _ => Vec::new(),
    }
}

/// An `Announce` frame of `announce`'s fields with these targets, built without its checks.
fn raw(targets: &[Target]) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend(5u64.to_be_bytes());
    p.extend([1; 32]);
    p.push(0);
    p.extend(9u64.to_be_bytes());
    p.push(u8::try_from(targets.len()).unwrap_or(u8::MAX));
    for t in targets {
        for n in [&t.name, &t.type_name] {
            p.extend(ok(u16::try_from(n.latin1().len())).to_be_bytes());
            p.extend(n.latin1());
        }
        p.push(t.format);
        p.extend(t.size.to_be_bytes());
        p.extend(t.sha256);
        match &t.inline {
            Some(i) => {
                p.push(1);
                p.extend(i);
            }
            None => p.push(0),
        }
    }
    frame(18, &p)
}

#[test]
fn announcements_keep_their_limits_both_ways() {
    let check = |targets: Vec<Target>, e: LimitError| {
        let m = announce(targets);
        refused_both_ways(&m, &raw(&targets_of(&m)), &e);
    };
    check(Vec::new(), LimitError::NoTargets);
    check(vec![text(None, 1), text(None, 2)], LimitError::DuplicateTarget);
    let mut odd = text(None, 1);
    odd.format = 7;
    check(vec![odd], LimitError::UnknownCode { code: 7 });
    // On the wire an inline target's bytes are its size, so only building can disagree.
    let m = announce(vec![text(Some(b"abc"), 4)]);
    let mut out = Vec::new();
    let e = LimitError::InlineSizeMismatch { size: 4, inline_len: 3 };
    assert_eq!(encode(&m, &mut out), Err(e));
    assert_eq!(decode(&raw(&targets_of(&m))), Err(DecodeError::Truncated));
    let big = vec![0; MAX_INLINE_LEN + 1];
    let mut over = text(Some(&big), u64::try_from(big.len()).unwrap_or(0));
    over.sha256 = [0; 32];
    // Too large is found before the bytes are read: the decoder waits for none of them.
    let m = announce(vec![over]);
    let mut out = Vec::new();
    assert_eq!(encode(&m, &mut out), Err(LimitError::InlineTooLarge { len: MAX_INLINE_LEN + 1 }));
    // Past 512 KiB inline in all: nine targets of 64 KiB.
    let full = vec![0; MAX_INLINE_LEN];
    let nine: Vec<Target> = (0..9)
        .map(|i| {
            let mut t = text(Some(&full), u64::try_from(full.len()).unwrap_or(0));
            t.name = name(&format!("t{i}"));
            t
        })
        .collect();
    let m = announce(nine);
    let mut out = Vec::new();
    assert_eq!(
        encode(&m, &mut out),
        Err(LimitError::InlineTooLarge { len: MAX_INLINE_TOTAL + MAX_INLINE_LEN })
    );
    // 255 targets is the most; long names past 1 MiB are the sender's to leave out.
    let many: Vec<Target> = (0..MAX_TARGETS)
        .map(|i| {
            let mut t = text(None, 1);
            t.name = name(&format!("{i:04}{}", "n".repeat(4092)));
            t
        })
        .collect();
    let mut out = Vec::new();
    let too_long = encode(&announce(many), &mut out);
    assert!(matches!(too_long, Err(LimitError::PayloadTooLarge { .. })), "{too_long:?}");
}

#[test]
fn an_inline_size_past_its_limit_is_refused_before_its_bytes() {
    // An inline flag whose size is 2^40: refused with the bytes not there, never waited for.
    let mut p = Vec::new();
    p.extend([0; 8]);
    p.extend([0; 32]);
    p.push(0);
    p.extend([0; 8]);
    p.push(1);
    for _ in 0..2 {
        p.extend([0, 1, b'a']);
    }
    p.push(8);
    p.extend((1u64 << 40).to_be_bytes());
    p.extend([0; 32]);
    p.push(1);
    let len = usize::try_from(1u64 << 40).unwrap_or(usize::MAX);
    assert_eq!(decode(&frame(18, &p)), Err(DecodeError::Limit(LimitError::InlineTooLarge { len })));
}

#[test]
fn forgets_hold_one_to_a_thousand_digests() {
    let m = Message::Forget { tick: 1, digests: Vec::new() };
    refused_both_ways(
        &m,
        &frame(20, &[0, 0, 0, 0, 0, 0, 0, 1, 0, 0]),
        &LimitError::TooManyEntries { count: 0 },
    );
    let most = Message::Forget { tick: 1, digests: vec![[3; 32]; MAX_FORGETS] };
    let b = bytes(&most);
    assert_eq!(decode(&b), Ok(Some((most, b.len()))));
    let mut over = vec![0, 0, 0, 0, 0, 0, 0, 1];
    over.extend(ok(u16::try_from(MAX_FORGETS + 1)).to_be_bytes());
    over.extend(vec![0; 32 * (MAX_FORGETS + 1)]);
    let m = Message::Forget { tick: 1, digests: vec![[0; 32]; MAX_FORGETS + 1] };
    let e = LimitError::TooManyEntries { count: MAX_FORGETS + 1 };
    let mut out = Vec::new();
    assert_eq!(encode(&m, &mut out), Err(e.clone()));
    // Past the frame's limit as well: refused from the header.
    assert!(decode(&frame(20, &over)).is_err());
}

#[test]
fn flags_are_zero_or_one() {
    assert_eq!(decode(&frame(22, &[2])), Err(DecodeError::BadFlag(2)));
    let mut p = vec![0; 8 + 32];
    p.push(2);
    assert_eq!(decode(&frame(18, &p)), Err(DecodeError::BadFlag(2)));
}

fn arb_name() -> impl Strategy<Value = Name> {
    proptest::collection::vec(1u8..=255, 1..12).prop_map(|b| ok(Name::new(&b)))
}

fn arb_target() -> impl Strategy<Value = Target> {
    (
        arb_name(),
        arb_name(),
        prop_oneof![Just(8u8), Just(16), Just(32)],
        any::<[u8; 32]>(),
        proptest::option::of(proptest::collection::vec(any::<u8>(), 0..40)),
        any::<u64>(),
    )
        .prop_map(|(name, type_name, format, sha256, inline, size)| {
            let size = inline.as_ref().map_or(size, |b| u64::try_from(b.len()).unwrap_or(0));
            Target { name, type_name, format, size, sha256, inline }
        })
}

fn arb_message() -> impl Strategy<Value = Message> {
    prop_oneof![
        (
            any::<u64>(),
            any::<[u8; 32]>(),
            proptest::option::of(any::<[u8; 32]>()),
            any::<u64>(),
            proptest::collection::vec(arb_target(), 1..6),
        )
            .prop_map(|(tick, digest, replaces, copied_at_ms, mut targets)| {
                targets.sort_by(|a, b| a.name.cmp(&b.name));
                targets.dedup_by(|a, b| a.name == b.name);
                announce_with(tick, digest, replaces, copied_at_ms, targets)
            }),
        any::<u64>().prop_map(|tick| Message::Emptied { tick }),
        (any::<u64>(), proptest::collection::vec(any::<[u8; 32]>(), 1..5))
            .prop_map(|(tick, digests)| Message::Forget { tick, digests }),
        (any::<[u8; 32]>(), arb_name()).prop_map(|(digest, name)| Message::Fetch { digest, name }),
        any::<bool>().prop_map(|held| Message::Answer { held }),
    ]
}

fn announce_with(
    tick: u64,
    digest: [u8; 32],
    replaces: Option<[u8; 32]>,
    copied_at_ms: u64,
    targets: Vec<Target>,
) -> Message {
    Message::Announce(Box::new(Announce { tick, digest, replaces, copied_at_ms, targets }))
}

proptest! {
    #[test]
    fn messages_round_trip(m in arb_message()) {
        let b = bytes(&m);
        prop_assert_eq!(decode(&b), Ok(Some((m, b.len()))));
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

    #[test]
    fn any_message_cut_short_waits_or_is_refused(m in arb_message(), cut in any::<prop::sample::Index>()) {
        let b = bytes(&m);
        let at = cut.index(b.len());
        prop_assert_eq!(check_decoder(b.get(..at).unwrap_or_default()), Ok(()));
    }
}
