use jackstay_proto::control::{
    Error, Kinds, MAX_LIST, MAX_PREVIEW_CHARS, MAX_SOURCE_LEN, MAX_THUMBNAIL_HEIGHT,
    MAX_THUMBNAIL_WIDTH, Request, Response, Status, Summary, Thumb, Thumbnail, VERSION,
    decode_request, decode_response, encode_request, encode_response, preview, printable,
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

fn request_bytes(r: &Request) -> Vec<u8> {
    let mut out = Vec::new();
    encode_request(r, &mut out);
    out
}

fn response_bytes(r: &Response) -> Vec<u8> {
    let mut out = Vec::new();
    ok(encode_response(r, &mut out));
    out
}

fn summary(id: u64) -> Summary {
    Summary {
        id,
        copied_at_ms: 1_761_300_000_000,
        pinned: true,
        current: false,
        kinds: Kinds(Kinds::TEXT | Kinds::IMAGE),
        size: 1234,
        source: Some("Firefox".into()),
        preview: "héllo wörld".into(),
    }
}

#[test]
fn the_layout_is_as_documented() {
    assert_eq!(request_bytes(&Request::Hello { version: VERSION }), [0, 0, 0, 2, 1, 0, 2]);
    assert_eq!(request_bytes(&Request::Clear), [0, 0, 0, 0, 7]);
    assert_eq!(
        request_bytes(&Request::Pin { id: 258, pinned: true }),
        [0, 0, 0, 9, 6, 0, 0, 0, 0, 0, 0, 1, 2, 1]
    );
    assert_eq!(response_bytes(&Response::Error(Error::NoSuchEntry)), [0, 0, 0, 1, 66, 3]);
    assert_eq!(
        request_bytes(&Request::Thumbnail { id: 5 }),
        [0, 0, 0, 8, 11, 0, 0, 0, 0, 0, 0, 0, 5]
    );
    let one = ok(Thumbnail::new(1, 1, 3, 2, vec![10, 20, 30, 40]));
    assert_eq!(
        response_bytes(&Response::Thumbnail { id: 5, thumbnail: Thumb::Ready(one) }),
        [
            0, 0, 0, 25, 71, 0, 0, 0, 0, 0, 0, 0, 5, 2, 0, 1, 0, 1, 0, 0, 0, 3, 0, 0, 0, 2, 10, 20,
            30, 40
        ]
    );
    assert_eq!(
        response_bytes(&Response::Thumbnail { id: 1, thumbnail: Thumb::Pending }),
        [0, 0, 0, 9, 71, 0, 0, 0, 0, 0, 0, 0, 1, 1]
    );
}

fn thumbnail(width: u16, height: u16) -> Thumbnail {
    let pixels = usize::from(width) * usize::from(height);
    let rgba = (0..pixels * 4).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect();
    ok(Thumbnail::new(width, height, u32::from(width) * 3, u32::from(height) * 3, rgba))
}

#[test]
fn every_request_and_response_round_trips() {
    let requests = [
        Request::Hello { version: 7 },
        Request::Status,
        Request::List { offset: 3, limit: 50 },
        Request::Select { id: 9 },
        Request::Delete { id: u64::MAX },
        Request::Pin { id: 1, pinned: false },
        Request::Clear,
        Request::ClearHistory,
        Request::ImportClipman { remove: true },
        Request::ImportClipman { remove: false },
        Request::Subscribe,
        Request::Thumbnail { id: 0 },
        Request::Thumbnail { id: u64::MAX },
    ];
    for r in requests {
        let bytes = request_bytes(&r);
        assert_eq!(decode_request(&bytes), Ok(Some((r, bytes.len()))), "{r:?}");
    }
    let errors = [
        Error::UnsupportedVersion,
        Error::BadSequence,
        Error::NoSuchEntry,
        Error::TooManyPinned,
        Error::StoreFailed,
        Error::ClaimFailed,
        Error::ImportFailed,
        Error::Internal,
    ];
    let mut responses = vec![
        Response::Welcome { version: VERSION },
        Response::Ok,
        Response::Status(Status {
            entries: 5,
            pinned: 2,
            current: Some(4),
            owns_clipboard: true,
            store_failing: false,
        }),
        Response::Status(Status {
            entries: 0,
            pinned: 0,
            current: None,
            owns_clipboard: false,
            store_failing: true,
        }),
        Response::List {
            total: 3,
            entries: vec![
                summary(1),
                Summary { source: None, preview: String::new(), ..summary(2) },
            ],
        },
        Response::List { total: 0, entries: vec![] },
        Response::Imported { added: 3, skipped: 1 },
        Response::Changed,
        Response::Thumbnail { id: 3, thumbnail: Thumb::Absent },
        Response::Thumbnail { id: 3, thumbnail: Thumb::Pending },
        Response::Thumbnail { id: 3, thumbnail: Thumb::Ready(thumbnail(7, 5)) },
        Response::Thumbnail {
            id: u64::MAX,
            thumbnail: Thumb::Ready(thumbnail(MAX_THUMBNAIL_WIDTH, MAX_THUMBNAIL_HEIGHT)),
        },
    ];
    responses.extend(errors.map(Response::Error));
    for r in responses {
        let bytes = response_bytes(&r);
        assert_eq!(decode_response(&bytes), Ok(Some((r.clone(), bytes.len()))), "{r:?}");
    }
}

#[test]
fn a_partial_frame_waits_and_a_bad_one_is_refused() {
    let bytes = response_bytes(&Response::List { total: 1, entries: vec![summary(1)] });
    for n in 0..bytes.len() {
        assert_eq!(decode_response(&bytes[..n]), Ok(None), "prefix of {n}");
    }
    assert_eq!(decode_request(&[0, 0, 0, 0, 99]), Err(DecodeError::UnknownKind(99)));
    assert_eq!(
        decode_response(&[0, 0, 0, 0, 1]),
        Err(DecodeError::UnknownKind(1)),
        "a request kind"
    );
    assert_eq!(
        decode_request(&[0, 0, 0, 0, 64]),
        Err(DecodeError::UnknownKind(64)),
        "a response kind"
    );
    assert_eq!(
        decode_request(&[0, 16, 0, 1, 2]),
        Err(DecodeError::FrameTooLarge { len: 0x0010_0001 })
    );
    assert_eq!(decode_request(&[0, 0, 0, 17]), Err(DecodeError::FrameTooLarge { len: 17 }));
    assert_eq!(decode_request(&[0, 0, 0, 16, 2]), Ok(None), "16 bytes is waited for");
    assert_eq!(
        decode_response(&[0, 16, 0, 1, 65]),
        Err(DecodeError::FrameTooLarge { len: 0x0010_0001 })
    );
    assert_eq!(decode_request(&[0, 0, 0, 1, 2, 0]), Err(DecodeError::TrailingBytes));
    assert_eq!(
        decode_request(&[0, 0, 0, 9, 6, 0, 0, 0, 0, 0, 0, 0, 1, 2]),
        Err(DecodeError::BadFlag(2))
    );
    assert_eq!(decode_request(&[0, 0, 0, 1, 1, 0]), Err(DecodeError::Truncated));
    assert_eq!(
        decode_response(&[0, 0, 0, 1, 66, 0]),
        Err(DecodeError::Limit(LimitError::UnknownCode { code: 0 }))
    );
    assert_eq!(
        decode_response(&[0, 0, 0, 1, 66, 9]),
        Err(DecodeError::Limit(LimitError::UnknownCode { code: 9 }))
    );
    // Two frames in one buffer: the first is decoded and its length reported.
    let mut two = request_bytes(&Request::Status);
    two.extend(request_bytes(&Request::Clear));
    assert_eq!(decode_request(&two), Ok(Some((Request::Status, 5))));
}

#[test]
fn limits_hold_on_both_sides() {
    let mut out = Vec::new();
    let too_many = Response::List { total: 0, entries: vec![summary(1); MAX_LIST + 1] };
    assert_eq!(
        encode_response(&too_many, &mut out),
        Err(LimitError::TooManyEntries { count: MAX_LIST + 1 })
    );
    let at_limit = Response::List { total: 0, entries: vec![summary(1); MAX_LIST] };
    assert!(encode_response(&at_limit, &mut out).is_ok());
    let long_source = Summary { source: Some("x".repeat(MAX_SOURCE_LEN + 1)), ..summary(1) };
    let r = Response::List { total: 1, entries: vec![long_source] };
    let mut out = Vec::new();
    assert_eq!(
        encode_response(&r, &mut out),
        Err(LimitError::TextTooLong { len: MAX_SOURCE_LEN + 1 })
    );
    assert!(out.is_empty(), "nothing appended on failure");
    let at_source = Summary { source: Some("x".repeat(MAX_SOURCE_LEN)), ..summary(1) };
    assert!(
        encode_response(&Response::List { total: 1, entries: vec![at_source] }, &mut out).is_ok()
    );
    let wide = "€".repeat(MAX_PREVIEW_CHARS);
    let r =
        Response::List { total: 1, entries: vec![Summary { preview: wide.clone(), ..summary(1) }] };
    let bytes = response_bytes(&r);
    assert_eq!(decode_response(&bytes).map(|d| d.is_some()), Ok(true), "200 wide characters fit");
    let r = Response::List {
        total: 1,
        entries: vec![Summary { preview: format!("{wide}x"), ..summary(1) }],
    };
    assert!(encode_response(&r, &mut Vec::new()).is_err());
    let r = Response::List { total: 1, entries: vec![Summary { kinds: Kinds(32), ..summary(1) }] };
    assert_eq!(encode_response(&r, &mut Vec::new()), Err(LimitError::UnknownCode { code: 32 }));
    let r = Response::List { total: 1, entries: vec![Summary { kinds: Kinds(31), ..summary(1) }] };
    assert!(encode_response(&r, &mut Vec::new()).is_ok());
}

#[test]
fn a_decoder_refuses_what_the_encoder_would_not_write() {
    let base = response_bytes(&Response::List { total: 1, entries: vec![summary(1)] });
    // Layout after the 5-byte header: total u32, count u16, then id, time, pinned, current, kinds.
    let kinds_at = 5 + 4 + 2 + 8 + 8 + 1 + 1;
    let mut bad = base.clone();
    bad[kinds_at] = 32;
    assert_eq!(
        decode_response(&bad),
        Err(DecodeError::Limit(LimitError::UnknownCode { code: 32 }))
    );
    let mut bad = base.clone();
    bad[9] = 0x01;
    bad[10] = 0xF5;
    assert_eq!(
        decode_response(&bad),
        Err(DecodeError::Limit(LimitError::TooManyEntries { count: 501 }))
    );
    // The preview's last byte made invalid UTF-8.
    let mut bad = base.clone();
    let last = bad.len() - 1;
    bad[last] = 0xFF;
    assert_eq!(decode_response(&bad), Err(DecodeError::Limit(LimitError::TextNotUtf8)));
    // A preview of 201 one-byte characters, within the byte limit, is still refused.
    let mut long = response_bytes(&Response::List {
        total: 1,
        entries: vec![Summary { preview: "x".repeat(MAX_PREVIEW_CHARS), ..summary(1) }],
    });
    long.push(b'x');
    let at = long.len() - MAX_PREVIEW_CHARS - 3;
    long[at..at + 2]
        .copy_from_slice(&u16::try_from(MAX_PREVIEW_CHARS + 1).unwrap_or(0).to_be_bytes());
    let len = u32::try_from(long.len() - 5).unwrap_or(0);
    long[..4].copy_from_slice(&len.to_be_bytes());
    assert_eq!(
        decode_response(&long),
        Err(DecodeError::Limit(LimitError::TextTooLong { len: 201 }))
    );
}

#[test]
fn previews_are_one_line_and_short() {
    assert_eq!(preview("a\nb\tc\u{7}"), "a b c ");
    assert_eq!(preview("x\u{1b}[2Jy"), "x [2Jy", "no terminal escapes");
    assert_eq!(preview("a\u{202e}b\u{2066}c\u{200f}d\u{202a}e\u{2069}"), "a b c d e ");
    assert_eq!(printable("a\u{61c}b\u{2028}c\u{feff}d\u{200b}e\u{e0041}f\u{ad}g"), "a b c d e f g");
    assert_eq!(printable("\u{1b}]0;title\u{7}"), " ]0;title ");
    assert_eq!(printable("ordinary ünïcode, 中文, emoji 🎉"), "ordinary ünïcode, 中文, emoji 🎉");
    assert_eq!(preview(&"é".repeat(300)).chars().count(), MAX_PREVIEW_CHARS);
    assert_eq!(preview(""), "");
}

fn arb_summary() -> impl Strategy<Value = Summary> {
    (
        any::<u64>(),
        any::<u64>(),
        any::<bool>(),
        any::<bool>(),
        0u8..32,
        any::<u64>(),
        proptest::option::of("[a-zA-Z0-9 ]{0,40}"),
        ".{0,200}",
    )
        .prop_map(|(id, copied_at_ms, pinned, current, kinds, size, source, preview)| {
            Summary {
                id,
                copied_at_ms,
                pinned,
                current,
                kinds: Kinds(kinds),
                size,
                source,
                preview,
            }
        })
}

proptest! {
    #[test]
    fn lists_round_trip(total in any::<u32>(), entries in proptest::collection::vec(arb_summary(), 0..20)) {
        let r = Response::List { total, entries };
        let bytes = response_bytes(&r);
        prop_assert_eq!(decode_response(&bytes), Ok(Some((r, bytes.len()))));
    }

    #[test]
    fn any_bytes_decode_without_panicking(bytes in proptest::collection::vec(any::<u8>(), 0..300)) {
        if let Ok(Some((r, used))) = decode_request(&bytes) {
            let again = request_bytes(&r);
            prop_assert_eq!(again.as_slice(), &bytes[..used]);
        }
        if let Ok(Some((r, used))) = decode_response(&bytes) {
            let again = response_bytes(&r);
            prop_assert_eq!(again.as_slice(), &bytes[..used]);
        }
    }
}

#[test]
fn limits_are_inclusive_when_decoding() {
    // A response of exactly the largest payload is waited for, not refused.
    assert_eq!(decode_response(&[0, 16, 0, 0, 65]), Ok(None));
    let at_list = Response::List { total: 0, entries: vec![summary(1); MAX_LIST] };
    let bytes = response_bytes(&at_list);
    assert_eq!(decode_response(&bytes), Ok(Some((at_list, bytes.len()))));
    let at_source = Response::List {
        total: 1,
        entries: vec![Summary { source: Some("x".repeat(MAX_SOURCE_LEN)), ..summary(1) }],
    };
    let bytes = response_bytes(&at_source);
    assert_eq!(decode_response(&bytes), Ok(Some((at_source, bytes.len()))));
    // One byte more in the source's length field than the limit allows.
    let one_more = Response::List {
        total: 1,
        entries: vec![Summary { source: Some("x".repeat(MAX_SOURCE_LEN - 1)), ..summary(1) }],
    };
    let mut bytes = response_bytes(&one_more);
    let at = 5 + 4 + 2 + 8 + 8 + 1 + 1 + 1 + 8 + 1;
    bytes[at..at + 2]
        .copy_from_slice(&u16::try_from(MAX_SOURCE_LEN + 1).unwrap_or(0).to_be_bytes());
    assert_eq!(
        decode_response(&bytes),
        Err(DecodeError::Limit(LimitError::TextTooLong { len: MAX_SOURCE_LEN + 1 }))
    );
}

#[test]
fn every_error_has_its_own_explanation() {
    use jackstay_proto::control::Error;
    let all = [
        Error::NoSuchEntry,
        Error::TooManyPinned,
        Error::StoreFailed,
        Error::ClaimFailed,
        Error::ImportFailed,
        Error::UnsupportedVersion,
        Error::Internal,
    ];
    let said: std::collections::HashSet<&str> = all.iter().map(|e| e.explain()).collect();
    assert_eq!(said.len(), all.len());
    assert!(said.iter().all(|s| !s.is_empty()));
    assert_eq!(Error::BadSequence.explain(), Error::UnsupportedVersion.explain());
}

#[test]
fn a_thumbnails_size_and_pixels_must_agree() {
    let new =
        |w: u16, h: u16, iw: u32, ih: u32, len: usize| Thumbnail::new(w, h, iw, ih, vec![0; len]);
    assert!(new(1, 1, 1, 1, 4).is_ok());
    assert!(new(MAX_THUMBNAIL_WIDTH, MAX_THUMBNAIL_HEIGHT, 128, 64, 128 * 64 * 4).is_ok());
    assert!(new(2, 3, u32::MAX, u32::MAX, 24).is_ok());
    for (w, h, iw, ih, len) in [
        (0, 1, 1, 1, 0),
        (1, 0, 1, 1, 0),
        (MAX_THUMBNAIL_WIDTH + 1, 1, 1000, 1, 129 * 4),
        (1, MAX_THUMBNAIL_HEIGHT + 1, 1, 1000, 65 * 4),
        (2, 1, 1, 1, 8),
        (1, 2, 1, 1, 8),
        (1, 1, 1, 1, 3),
        (1, 1, 1, 1, 5),
        (2, 2, 2, 2, 4),
    ] {
        assert_eq!(
            new(w, h, iw, ih, len),
            Err(LimitError::BadThumbnail),
            "{w}x{h} of {iw}x{ih}, {len} bytes"
        );
    }
    let t = thumbnail(2, 1);
    assert_eq!(format!("{t:?}"), "Thumbnail(2x1 of 6x3)", "no pixels in Debug");
    let mut bytes = Vec::new();
    t.encode(&mut bytes);
    assert_eq!(bytes.len(), 12 + 8);
    assert_eq!(Thumbnail::decode(&bytes), Ok(t.clone()));
    assert_eq!(Thumbnail::decode(&bytes[..bytes.len() - 1]), Err(DecodeError::Truncated));
    bytes.push(0);
    assert_eq!(Thumbnail::decode(&bytes), Err(DecodeError::TrailingBytes));
    assert_eq!(Thumbnail::MAX_ENCODED_LEN, 12 + 128 * 64 * 4);
}

#[test]
fn a_thumbnail_reply_is_refused_when_it_breaks_a_limit() {
    let base =
        response_bytes(&Response::Thumbnail { id: 1, thumbnail: Thumb::Ready(thumbnail(2, 1)) });
    let state_at = 5 + 8;
    let mut bad = base.clone();
    bad[state_at] = 3;
    assert_eq!(decode_response(&bad), Err(DecodeError::Limit(LimitError::UnknownCode { code: 3 })));
    // Wider than the limit, with the pixels to match: refused all the same.
    let mut wide = base[..=state_at].to_vec();
    wide.extend_from_slice(&(MAX_THUMBNAIL_WIDTH + 1).to_be_bytes());
    wide.extend_from_slice(&1u16.to_be_bytes());
    wide.extend_from_slice(&1000u32.to_be_bytes());
    wide.extend_from_slice(&1u32.to_be_bytes());
    wide.extend(std::iter::repeat_n(0, usize::from(MAX_THUMBNAIL_WIDTH + 1) * 4));
    let len = u32::try_from(wide.len() - 5).unwrap_or(0);
    wide[..4].copy_from_slice(&len.to_be_bytes());
    assert!(decode_response(&wide).is_err());
    // An image smaller than its thumbnail.
    let mut small = base.clone();
    small[state_at + 1 + 4..state_at + 1 + 8].copy_from_slice(&1u32.to_be_bytes());
    assert_eq!(decode_response(&small), Err(DecodeError::Limit(LimitError::BadThumbnail)));
    // One pixel byte short, or one over.
    let mut short = base.clone();
    short.pop();
    let len = u32::try_from(short.len() - 5).unwrap_or(0);
    short[..4].copy_from_slice(&len.to_be_bytes());
    assert_eq!(decode_response(&short), Err(DecodeError::Truncated));
    let mut long = base.clone();
    long.push(0);
    let len = u32::try_from(long.len() - 5).unwrap_or(0);
    long[..4].copy_from_slice(&len.to_be_bytes());
    assert_eq!(decode_response(&long), Err(DecodeError::TrailingBytes));
}

fn arb_thumbnail() -> impl Strategy<Value = Thumbnail> {
    (1..=MAX_THUMBNAIL_WIDTH, 1..=MAX_THUMBNAIL_HEIGHT, 1u32..4, 1u32..4, any::<u8>()).prop_map(
        |(w, h, sx, sy, seed)| {
            let len = usize::from(w) * usize::from(h) * 4;
            let rgba = (0..len).map(|i| u8::try_from(i % 256).unwrap_or(0) ^ seed).collect();
            ok(Thumbnail::new(w, h, u32::from(w) * sx, u32::from(h) * sy, rgba))
        },
    )
}

proptest! {
    #[test]
    fn thumbnails_round_trip(id in any::<u64>(), t in arb_thumbnail()) {
        let r = Response::Thumbnail { id, thumbnail: Thumb::Ready(t) };
        let bytes = response_bytes(&r);
        prop_assert_eq!(decode_response(&bytes), Ok(Some((r, bytes.len()))));
    }
}
