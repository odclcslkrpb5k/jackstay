use jackstay_proto::{
    Announce, DecodeError, EntryId, FRAME_HEADER_LEN, LimitError, MAX_INLINE_LEN, MAX_PAYLOAD_LEN,
    MAX_TARGET_NAME_LEN, MAX_TARGETS, Message, Target, TargetName, decode, encode,
};
use proptest::prelude::*;

fn ok<T, E: std::fmt::Debug>(r: Result<T, E>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("expected Ok, got Err({e:?})"),
    }
}

fn name(s: &str) -> TargetName {
    ok(TargetName::parse(s.as_bytes()))
}

fn sample() -> Message {
    Message::Announce(ok(Announce::new(
        EntryId::new(7),
        vec![
            ok(Target::inline(name("UTF8_STRING"), b"hello".to_vec())),
            Target::deferred(name("image/png"), 3_000_000),
        ],
    )))
}

fn framed(message: &Message) -> Vec<u8> {
    let mut out = Vec::new();
    encode(message, &mut out);
    out
}

/// A frame around a hand-built payload, so tests can say exactly what is on the wire.
fn raw_frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = ok(u32::try_from(payload.len())).to_be_bytes().to_vec();
    out.push(kind);
    out.extend_from_slice(payload);
    out
}

/// An announce payload with one target, built field by field.
fn announce_payload(target_name: &[u8], size: u64, inline: Option<(u32, &[u8])>) -> Vec<u8> {
    let mut p = 1u64.to_be_bytes().to_vec();
    p.push(1);
    p.push(ok(u8::try_from(target_name.len())));
    p.extend_from_slice(target_name);
    p.extend_from_slice(&size.to_be_bytes());
    match inline {
        None => p.push(0),
        Some((claimed_len, bytes)) => {
            p.push(1);
            p.extend_from_slice(&claimed_len.to_be_bytes());
            p.extend_from_slice(bytes);
        }
    }
    p
}

#[test]
fn round_trips() {
    let message = sample();
    let bytes = framed(&message);
    assert_eq!(decode(&bytes), Ok(Some((message, bytes.len()))));
}

#[test]
fn every_proper_prefix_is_incomplete() {
    let bytes = framed(&sample());
    for end in 0..bytes.len() {
        assert_eq!(decode(bytes.get(..end).unwrap_or_default()), Ok(None), "prefix of {end} bytes");
    }
}

#[test]
fn oversized_frame_is_refused_from_its_header_alone() {
    let header = [0x80, 0, 0, 0];
    assert_eq!(decode(&header), Err(DecodeError::FrameTooLarge { len: 0x8000_0000 }));
}

#[test]
fn one_byte_over_the_limit_is_refused_from_its_header_alone() {
    let len = MAX_PAYLOAD_LEN + 1;
    assert_eq!(decode(&len.to_be_bytes()), Err(DecodeError::FrameTooLarge { len }));
}

#[test]
fn duplicate_target_names_are_refused() {
    let mut payload = 1u64.to_be_bytes().to_vec();
    payload.push(2);
    for _ in 0..2 {
        payload.push(11);
        payload.extend_from_slice(b"UTF8_STRING");
        payload.extend_from_slice(&1u64.to_be_bytes());
        payload.push(0);
    }
    assert_eq!(
        decode(&raw_frame(1, &payload)),
        Err(DecodeError::Limit(LimitError::DuplicateTarget { name: name("UTF8_STRING") }))
    );
}

#[test]
fn largest_allowed_frame_is_only_incomplete() {
    assert_eq!(decode(&MAX_PAYLOAD_LEN.to_be_bytes()), Ok(None));
}

// The shape of CVE-2026-41476: the outer frame length is valid, but an inner length claims more
// bytes than the frame holds.
#[test]
fn inner_length_beyond_the_frame_is_refused() {
    let payload = announce_payload(b"UTF8_STRING", 5, Some((60_000, b"hello")));
    assert_eq!(decode(&raw_frame(1, &payload)), Err(DecodeError::Truncated));
}

#[test]
fn inner_length_over_the_inline_limit_is_refused() {
    let claimed = ok(u32::try_from(MAX_INLINE_LEN + 1));
    let payload = announce_payload(b"UTF8_STRING", 5, Some((claimed, b"hello")));
    assert_eq!(
        decode(&raw_frame(1, &payload)),
        Err(DecodeError::Limit(LimitError::InlineTooLarge { len: MAX_INLINE_LEN + 1 }))
    );
}

#[test]
fn inline_size_must_match_its_bytes() {
    let payload = announce_payload(b"UTF8_STRING", 6, Some((5, b"hello")));
    assert_eq!(
        decode(&raw_frame(1, &payload)),
        Err(DecodeError::Limit(LimitError::InlineSizeMismatch { size: 6, inline_len: 5 }))
    );
}

#[test]
fn bytes_after_the_message_inside_its_frame_are_refused() {
    let mut payload = announce_payload(b"UTF8_STRING", 1, None);
    payload.push(0);
    assert_eq!(decode(&raw_frame(1, &payload)), Err(DecodeError::TrailingBytes));
}

#[test]
fn unknown_kind_is_refused() {
    assert_eq!(decode(&raw_frame(200, &[])), Err(DecodeError::UnknownKind(200)));
}

#[test]
fn inline_flag_must_be_zero_or_one() {
    let mut payload = announce_payload(b"UTF8_STRING", 1, None);
    if let Some(flag) = payload.last_mut() {
        *flag = 2;
    }
    assert_eq!(decode(&raw_frame(1, &payload)), Err(DecodeError::BadFlag(2)));
}

#[test]
fn target_names_are_printable_ascii_with_spaces() {
    let libreoffice =
        br#"application/x-openoffice-embed-source-xml;windows_formatname="Star Embed Source (XML)""#;
    assert!(TargetName::parse(libreoffice).is_ok());
    let payload = announce_payload(b"UTF8\tSTRING", 1, None);
    assert_eq!(
        decode(&raw_frame(1, &payload)),
        Err(DecodeError::Limit(LimitError::TargetNameNotPrintableAscii))
    );
    assert_eq!(TargetName::parse(b""), Err(LimitError::TargetNameEmpty));
    assert_eq!(TargetName::parse("é".as_bytes()), Err(LimitError::TargetNameNotPrintableAscii));
}

#[test]
fn target_count_is_limited_when_decoding() {
    let mut payload = 1u64.to_be_bytes().to_vec();
    payload.push(ok(u8::try_from(MAX_TARGETS + 1)));
    assert_eq!(
        decode(&raw_frame(1, &payload)),
        Err(DecodeError::Limit(LimitError::TooManyTargets { count: MAX_TARGETS + 1 }))
    );
}

#[test]
fn an_announcement_that_would_not_fit_a_frame_cannot_be_built() {
    let big = |i| ok(Target::inline(name(&format!("image/x-{i}")), vec![0; MAX_INLINE_LEN]));
    let targets: Vec<Target> = (0..17).map(big).collect();
    assert!(matches!(
        Announce::new(EntryId::new(1), targets),
        Err(LimitError::PayloadTooLarge { .. })
    ));
}

fn arb_target() -> impl Strategy<Value = Target> {
    let name = "[ -~]{1,40}".prop_map(|s| ok(TargetName::parse(s.as_bytes())));
    prop_oneof![
        (name.clone(), any::<u64>()).prop_map(|(n, size)| Target::deferred(n, size)),
        (name, proptest::collection::vec(any::<u8>(), 0..512))
            .prop_map(|(n, data)| ok(Target::inline(n, data))),
    ]
}

fn arb_message() -> impl Strategy<Value = Message> {
    (any::<u64>(), proptest::collection::vec(arb_target(), 1..=MAX_TARGETS)).prop_map(
        |(id, mut targets)| {
            let mut seen = std::collections::HashSet::new();
            targets.retain(|t| seen.insert(t.name().clone()));
            Message::Announce(ok(Announce::new(EntryId::new(id), targets)))
        },
    )
}

/// A frame whose header is consistent with the bytes that follow, so the payload parser, not the
/// frame header, is what meets the arbitrary input.
fn arb_consistent_frame() -> impl Strategy<Value = Vec<u8>> {
    (any::<u8>(), proptest::collection::vec(any::<u8>(), 0..1024))
        .prop_map(|(kind, payload)| raw_frame(kind % 3, &payload))
}

/// A valid encoding with one byte changed, a byte inserted, or a tail cut off.
fn arb_damaged_frame() -> impl Strategy<Value = Vec<u8>> {
    (arb_message(), any::<prop::sample::Index>(), any::<u8>(), 0u8..3).prop_map(
        |(message, at, byte, how)| {
            let mut bytes = framed(&message);
            let i = at.index(bytes.len());
            match how {
                0 => {
                    if let Some(b) = bytes.get_mut(i) {
                        *b ^= byte | 1;
                    }
                }
                1 => bytes.insert(i, byte),
                _ => bytes.truncate(i),
            }
            bytes
        },
    )
}

proptest! {
    #[test]
    fn decoding_arbitrary_bytes_never_panics_and_never_overreads(
        bytes in proptest::collection::vec(any::<u8>(), 0..2048)
    ) {
        if let Ok(Some((_, used))) = decode(&bytes) {
            prop_assert!(used >= FRAME_HEADER_LEN && used <= bytes.len());
        }
    }

    #[test]
    fn payload_parser_never_panics_on_consistent_frames(bytes in arb_consistent_frame()) {
        if let Ok(Some((_, used))) = decode(&bytes) {
            prop_assert_eq!(used, bytes.len());
        }
    }

    #[test]
    fn damaged_valid_frames_never_panic_and_decode_only_within_bounds(
        bytes in arb_damaged_frame()
    ) {
        if let Ok(Some((Message::Announce(a), used))) = decode(&bytes) {
            prop_assert!(used <= bytes.len());
            prop_assert!(!a.targets().is_empty() && a.targets().len() <= MAX_TARGETS);
        }
    }

    #[test]
    fn valid_messages_round_trip(message in arb_message()) {
        let bytes = framed(&message);
        prop_assert_eq!(decode(&bytes), Ok(Some((message, bytes.len()))));
    }

    #[test]
    fn concatenated_frames_decode_in_order(a in arb_message(), b in arb_message()) {
        let mut bytes = framed(&a);
        let first_len = bytes.len();
        encode(&b, &mut bytes);
        prop_assert_eq!(decode(&bytes), Ok(Some((a, first_len))));
        let rest = bytes.get(first_len..).unwrap_or_default();
        prop_assert_eq!(decode(rest), Ok(Some((b, rest.len()))));
    }
}

#[test]
fn accessors_return_what_was_built() {
    let Message::Announce(a) = sample();
    assert_eq!(a.id(), EntryId::new(7));
    assert_eq!(a.id().get(), 7);
    let [text, image] = a.targets() else { panic!("expected two targets") };
    assert_eq!(
        (text.name().as_str(), text.size(), text.inline_data()),
        ("UTF8_STRING", 5, Some(&b"hello"[..]))
    );
    assert_eq!(
        (image.name().as_str(), image.size(), image.inline_data()),
        ("image/png", 3_000_000, None)
    );
}

#[test]
fn target_name_limit_is_inclusive() {
    let longest = vec![b'x'; MAX_TARGET_NAME_LEN];
    assert!(TargetName::parse(&longest).is_ok());
    let payload = announce_payload(&longest, 1, None);
    assert!(matches!(decode(&raw_frame(1, &payload)), Ok(Some(_))));
    let too_long = vec![b'x'; MAX_TARGET_NAME_LEN + 1];
    assert_eq!(
        TargetName::parse(&too_long),
        Err(LimitError::TargetNameTooLong { len: MAX_TARGET_NAME_LEN + 1 })
    );
}

#[test]
fn inline_limit_is_inclusive_when_decoding() {
    let data = vec![7; MAX_INLINE_LEN];
    let len = ok(u32::try_from(MAX_INLINE_LEN));
    let payload = announce_payload(b"image/png", u64::from(len), Some((len, &data)));
    let Ok(Some((Message::Announce(a), _))) = decode(&raw_frame(1, &payload)) else {
        panic!("a target of exactly MAX_INLINE_LEN must decode")
    };
    assert_eq!(a.targets().first().and_then(Target::inline_data), Some(data.as_slice()));
}

#[test]
fn errors_describe_themselves() {
    assert_eq!(DecodeError::FrameTooLarge { len: 9 }.to_string(), "frame payload of 9 bytes");
    assert_eq!(DecodeError::BadFlag(2).to_string(), "flag byte 2");
    assert_eq!(
        DecodeError::Limit(LimitError::DuplicateTarget { name: name("TEXT") }).to_string(),
        "duplicate target TEXT"
    );
    assert_eq!(LimitError::InlineTooLarge { len: 3 }.to_string(), "inline data of 3 bytes");
}
