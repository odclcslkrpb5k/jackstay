use jackstay_clip::{EntryId, Format, NotKept, Source, TargetName};
use jackstay_store::{
    BlobHash, HeaderError, Index, MAX_ID, Parsed, SavedData, SavedEntry, SavedTarget, encode,
    invariants, parse,
};
use proptest::prelude::*;

fn ok<T, E: std::fmt::Debug>(r: Result<T, E>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("expected Ok, got Err({e:?})"),
    }
}

fn name(s: &[u8]) -> TargetName {
    ok(TargetName::from_latin1(s))
}

fn kept(n: &str, bytes: &[u8]) -> SavedTarget {
    SavedTarget {
        name: name(n.as_bytes()),
        data: SavedData::Kept {
            type_name: name(n.as_bytes()),
            format: Format::Bits8,
            blob: BlobHash::of(bytes),
        },
    }
}

fn entry(id: u64, targets: Vec<SavedTarget>) -> SavedEntry {
    SavedEntry {
        id: EntryId::new(id),
        copied_at_ms: 1_761_300_000_000,
        pinned: false,
        source: Source::Local { owner_class: None },
        targets,
    }
}

fn index(entries: Vec<SavedEntry>) -> Index {
    Index { next_id: 100, cleared: false, entries }
}

fn parsed(bytes: &[u8]) -> Parsed {
    ok(parse(bytes))
}

fn text(i: &Index) -> String {
    String::from_utf8_lossy(&encode(i)).into_owned()
}

#[test]
fn the_format_is_as_documented() {
    let mut e = entry(42, vec![kept("UTF8_STRING", b"hi")]);
    e.pinned = true;
    e.source = Source::Local { owner_class: Some("Firefox".into()) };
    e.targets.push(SavedTarget {
        name: name(b"image/png"),
        data: SavedData::NotKept(NotKept::TooLarge),
    });
    let i = Index { next_id: 43, cleared: true, entries: vec![e] };
    let hash = BlobHash::of(b"hi").hex();
    assert_eq!(
        text(&i),
        format!(
            "jackstay-index 1\nnext-id 43\ncleared 1\nentry 42 1761300000000 1\nsource local \
             Firefox\ntarget UTF8_STRING kept UTF8_STRING 8 {hash}\ntarget image/png not-kept \
             too-large\nend\n"
        )
    );
    assert_eq!(
        parsed(&encode(&i)),
        Parsed { index: i, dropped: vec![], complete: true, next_id_raised: false }
    );
}

#[test]
fn awkward_names_and_classes_round_trip() {
    let libreoffice =
        br#"application/x-openoffice-embed-source-xml;windows_formatname="Star Embed Source (XML)""#;
    let mut e = entry(1, vec![kept("UTF8_STRING", b"x")]);
    e.targets.push(SavedTarget {
        name: name(libreoffice),
        data: SavedData::Kept {
            type_name: name(&[b'%', 0xE9, b' ', b'\n', 0xFF]),
            format: Format::Bits32,
            blob: BlobHash::of(b""),
        },
    });
    e.source = Source::Local { owner_class: Some(String::new()) };
    let mut f = entry(2, vec![kept("text/plain", b"y")]);
    f.source = Source::Local { owner_class: Some("Ünïcode name\twith tab %".into()) };
    let i = index(vec![e, f]);
    let bytes = encode(&i);
    assert!(!bytes.contains(&b'\t') && bytes.iter().all(|&b| b < 0x80), "{}", text(&i));
    assert_eq!(parsed(&bytes).index, i);
}

#[test]
fn blob_hashes_are_sha256_in_lowercase_hex() {
    assert_eq!(
        BlobHash::of(b"abc").hex(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let h = BlobHash::of(b"abc");
    assert_eq!(BlobHash::from_hex(h.hex().as_bytes()), Some(h));
    assert_eq!(BlobHash::from_hex(h.hex().to_uppercase().as_bytes()), None);
    assert_eq!(BlobHash::from_hex(&h.hex().as_bytes()[..62]), None);
    assert_eq!(BlobHash::from_hex(&h.hex().as_bytes()[..63]), None);
    assert_eq!(BlobHash::from_hex(format!("{}00", h.hex()).as_bytes()), None);
    assert_eq!(BlobHash::from_hex(b"g".repeat(64).as_slice()), None);
    assert_eq!(format!("{h:?}"), format!("BlobHash({})", h.hex()));
}

#[test]
fn a_bad_header_refuses_the_file() {
    assert_eq!(parse(b""), Err(HeaderError::NotAnIndex));
    assert_eq!(parse(b"hello\n"), Err(HeaderError::NotAnIndex));
    assert_eq!(
        parse(b"jackstay-index 2\nnext-id 1\ncleared 0\nend\n"),
        Err(HeaderError::UnknownVersion)
    );
    assert_eq!(
        parse(b"jackstay-index 01\nnext-id 1\ncleared 0\nend\n"),
        Err(HeaderError::UnknownVersion)
    );
    assert_eq!(parse(b"jackstay-index\nnext-id 1\ncleared 0\nend\n"), Err(HeaderError::Malformed));
    assert_eq!(
        parse(b"jackstay-index 1\nnext-id x\ncleared 0\nend\n"),
        Err(HeaderError::Malformed)
    );
    assert_eq!(parse(b"jackstay-index 1\nnext 1\ncleared 0\nend\n"), Err(HeaderError::Malformed));
    assert_eq!(
        parse(b"jackstay-index 1\nnext-id 1\ncleared 2\nend\n"),
        Err(HeaderError::Malformed)
    );
    assert_eq!(parse(b"jackstay-index 1\nnext-id 1\nclear 0\nend\n"), Err(HeaderError::Malformed));
    assert_eq!(parse(b"jackstay-index 1\nnext-id 1\n"), Err(HeaderError::Malformed));
    assert_eq!(HeaderError::NotAnIndex.to_string(), "not a jackstay index");
    let p = parsed(b"jackstay-index 1\nnext-id 7\ncleared 1\nend\n");
    assert_eq!(
        p,
        Parsed {
            index: Index { next_id: 7, cleared: true, entries: vec![] },
            dropped: vec![],
            complete: true,
            next_id_raised: false,
        }
    );
}

/// Replaces line `n` (from 0) of `i`'s encoding.
fn with_line(i: &Index, n: usize, line: &str) -> Vec<u8> {
    let t = text(i);
    let mut lines: Vec<&str> = t.lines().collect();
    lines[n] = line;
    format!("{}\n", lines.join("\n")).into_bytes()
}

#[test]
fn a_bad_record_is_dropped_and_the_rest_kept() {
    let i = index(vec![
        entry(3, vec![kept("UTF8_STRING", b"c")]),
        entry(2, vec![kept("UTF8_STRING", b"b")]),
        entry(1, vec![kept("UTF8_STRING", b"a")]),
    ]);
    // Lines: 0-2 header; record n (from 1) is lines 3n, 3n+1, 3n+2; then end.
    let bad = [
        (3, "entry 3 1761300000000"),
        (3, "entry 03 1761300000000 0"),
        (3, "entry 18446744073709551615 1 0"),
        (3, "entry 3 1 2"),
        (4, "source remote"),
        (4, "source local a b"),
        (4, "source local %FF"),
        (4, "source local %e9"),
        (4, "source local %41"),
        (4, "source local a%"),
        (4, "source local a%4"),
        (4, "source local a\u{e9}"),
        (4, "garbage"),
        (4, ""),
        (5, "target UTF8_STRING kept UTF8_STRING 7 00"),
        (5, "target UTF8_STRING kept UTF8_STRING 08 00"),
        (5, "target UTF8_STRING not-kept tired"),
        (5, "target UTF8_STRING copied"),
        (5, "target %00 not-kept too-large"),
        (5, "target UTF8_STRING not-kept too-large"),
        (5, "target  not-kept too-large"),
        (5, "source local"),
    ];
    for (line, replacement) in bad {
        let p = parsed(&with_line(&i, line, replacement));
        assert_eq!(p.dropped, vec![1], "{replacement:?}");
        let ids: Vec<u64> = p.index.entries.iter().map(|e| e.id.get()).collect();
        assert_eq!(ids, vec![2, 1], "{replacement:?}");
        assert!(p.complete, "{replacement:?}");
    }
}

#[test]
fn records_must_be_whole_and_ids_unique() {
    let hash = BlobHash::of(b"x").hex();
    let t = format!("target UTF8_STRING kept UTF8_STRING 8 {hash}");
    let file = |records: &str| format!("jackstay-index 1\nnext-id 9\ncleared 0\n{records}end\n");
    let ids = |p: &Parsed| p.index.entries.iter().map(|e| e.id.get()).collect::<Vec<_>>();
    // No source; no target; nothing kept; a name twice; an id twice.
    let p = parsed(file(&format!("entry 1 0 0\n{t}\n")).as_bytes());
    assert_eq!((ids(&p), p.dropped), (vec![], vec![1]));
    let p = parsed(file("entry 1 0 0\nsource local\n").as_bytes());
    assert_eq!((ids(&p), p.dropped), (vec![], vec![1]));
    let p = parsed(file("entry 1 0 0\nsource local\ntarget a not-kept too-large\n").as_bytes());
    assert_eq!((ids(&p), p.dropped), (vec![], vec![1]));
    let p = parsed(file(&format!("entry 1 0 0\nsource local\n{t}\n{t}\n")).as_bytes());
    assert_eq!((ids(&p), p.dropped), (vec![], vec![1]));
    let p = parsed(
        file(&format!("entry 1 0 0\nsource local\n{t}\nentry 1 0 0\nsource local\n{t}\n"))
            .as_bytes(),
    );
    assert_eq!((ids(&p), p.dropped), (vec![1], vec![2]));
    let p = parsed(file(&format!("entry 1 0 0\nsource local\nsource local x\n{t}\n")).as_bytes());
    assert_eq!((ids(&p), p.dropped), (vec![], vec![1]), "two sources");
    let p = parsed(file(&format!("entry 1 0 0\nsource local x\nsource local\n{t}\n")).as_bytes());
    assert_eq!((ids(&p), p.dropped), (vec![], vec![1]), "two sources");
    // A line before any record.
    let p = parsed(file(&format!("stray\nstray\nentry 1 0 0\nsource local\n{t}\n")).as_bytes());
    assert_eq!((ids(&p), p.dropped), (vec![1], vec![0]));
}

#[test]
fn a_cut_short_file_keeps_what_it_has_and_says_so() {
    let i = index(vec![
        entry(2, vec![kept("UTF8_STRING", b"b")]),
        entry(1, vec![kept("UTF8_STRING", b"a")]),
    ]);
    let full = encode(&i);
    let p = parsed(&full[..full.len() - 1]);
    assert_eq!((p.index.entries.len(), p.complete), (2, false), "no final newline");
    let p = parsed(&full[..full.len() - 4]);
    assert_eq!((p.index.entries.len(), p.complete), (1, false), "no end line");
    assert_eq!(p.dropped, vec![2], "the last record may be cut short, so it is dropped");
    let mut extra = full.clone();
    extra.extend_from_slice(b"entry 9 0 0\n");
    let p = parsed(&extra);
    assert_eq!((p.index.entries.len(), p.complete), (2, false), "something after the end");
    // Every prefix parses without panicking, and never yields an entry that is not whole.
    let mut two = entry(3, vec![kept("UTF8_STRING", b"c")]);
    two.targets.push(SavedTarget {
        name: name(b"image/png"),
        data: SavedData::NotKept(NotKept::TooLarge),
    });
    let i = index(vec![two, entry(2, vec![kept("UTF8_STRING", b"b")])]);
    let full = encode(&i);
    for n in 0..full.len() {
        if let Ok(p) = parse(&full[..n]) {
            assert!(!p.complete || n == full.len());
            assert!(p.index.entries.iter().all(|e| i.entries.contains(e)));
        }
    }
}

#[test]
fn next_id_is_raised_above_every_id_read() {
    let i = Index { next_id: 2, cleared: false, entries: vec![entry(5, vec![kept("a", b"a")])] };
    let p = parsed(&encode(&i));
    assert_eq!((p.index.next_id, p.next_id_raised), (6, true));
    let i = Index { next_id: 50, cleared: false, entries: vec![entry(5, vec![kept("a", b"a")])] };
    let p = parsed(&encode(&i));
    assert_eq!((p.index.next_id, p.next_id_raised), (50, false));
    // A dropped record's id counts too, so it is never issued again.
    let file = b"jackstay-index 1\nnext-id 2\ncleared 0\nentry 9 0 0\nsource local\nend\n";
    let p = parsed(file);
    assert_eq!((p.dropped, p.index.next_id, p.next_id_raised), (vec![1], 10, true));
}

#[test]
fn ids_from_disk_stay_far_from_the_end_of_ids() {
    let header = |n: u64| format!("jackstay-index 1\nnext-id {n}\ncleared 0\nend\n");
    assert_eq!(parsed(header(MAX_ID).as_bytes()).index.next_id, MAX_ID);
    assert_eq!(parse(header(MAX_ID + 1).as_bytes()), Err(HeaderError::Malformed));
    assert_eq!(parse(header(u64::MAX).as_bytes()), Err(HeaderError::Malformed));
    let i = index(vec![entry(MAX_ID - 1, vec![kept("a", b"a")])]);
    let p = parsed(&encode(&i));
    assert_eq!((p.index.entries.len(), p.index.next_id), (1, MAX_ID));
    let i = Index {
        next_id: MAX_ID,
        cleared: false,
        entries: vec![entry(MAX_ID, vec![kept("a", b"a")])],
    };
    let p = parsed(&encode(&i));
    assert_eq!((p.index.entries.len(), p.dropped, p.index.next_id), (0, vec![1], MAX_ID));
}

fn arb_name() -> impl Strategy<Value = TargetName> {
    proptest::collection::vec(1u8..=255, 1..12).prop_map(|b| name(&b))
}

fn arb_target() -> impl Strategy<Value = SavedTarget> {
    let data = prop_oneof![
        (arb_name(), 0u8..3, any::<[u8; 4]>()).prop_map(|(type_name, f, b)| SavedData::Kept {
            type_name,
            format: [Format::Bits8, Format::Bits16, Format::Bits32][usize::from(f)],
            blob: BlobHash::of(&b),
        }),
        (0u8..3).prop_map(|r| SavedData::NotKept(
            [NotKept::NotCanonical, NotKept::TooLarge, NotKept::OwnerFailed][usize::from(r)]
        )),
    ];
    (arb_name(), data).prop_map(|(name, data)| SavedTarget { name, data })
}

fn arb_entry() -> impl Strategy<Value = SavedEntry> {
    (
        0u64..MAX_ID,
        any::<u64>(),
        any::<bool>(),
        proptest::option::of(".{0,8}"),
        proptest::collection::vec(arb_target(), 0..4),
        arb_name(),
    )
        .prop_map(|(id, copied_at_ms, pinned, owner_class, mut targets, kept_name)| {
            targets.retain(|t| t.name != kept_name);
            targets.push(SavedTarget {
                name: kept_name,
                data: SavedData::Kept {
                    type_name: name(b"T"),
                    format: Format::Bits8,
                    blob: BlobHash::of(b""),
                },
            });
            let mut seen = std::collections::HashSet::new();
            targets.retain(|t| seen.insert(t.name.clone()));
            SavedEntry {
                id: EntryId::new(id),
                copied_at_ms,
                pinned,
                source: Source::Local { owner_class },
                targets,
            }
        })
}

proptest! {
    #[test]
    fn every_valid_index_round_trips(
        next_id in 0..=MAX_ID,
        cleared in any::<bool>(),
        entries in proptest::collection::vec(arb_entry(), 0..6),
    ) {
        let mut seen = std::collections::HashSet::new();
        let entries: Vec<SavedEntry> = entries.into_iter().filter(|e| seen.insert(e.id)).collect();
        let highest = entries.iter().map(|e| e.id.get()).max();
        let next_id = highest.map_or(next_id, |h| next_id.max(h + 1));
        let i = Index { next_id, cleared, entries };
        let bytes = encode(&i);
        prop_assert_eq!(parse(&bytes), Ok(Parsed { index: i.clone(), dropped: vec![], complete: true, next_id_raised: false }));
        prop_assert_eq!(invariants::index(&bytes), Ok(true));
    }

    #[test]
    fn any_bytes_hold_the_invariants(bytes in proptest::collection::vec(any::<u8>(), 0..400)) {
        let mut file = b"jackstay-index 1\nnext-id 3\ncleared 0\n".to_vec();
        file.extend_from_slice(&bytes);
        prop_assert_eq!(invariants::index(&file).err(), None);
        prop_assert_eq!(invariants::index(&bytes).err(), None);
    }
}
