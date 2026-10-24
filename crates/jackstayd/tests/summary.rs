use std::sync::Arc;

use jackstay_clip::{
    Content, Entry, EntryId, Format, Kept, NotKept, Source, Target, TargetData, TargetName,
};
use jackstay_proto::control::{Kinds, MAX_PREVIEW_CHARS, MAX_SOURCE_LEN};
use jackstayd::summary::summary;

fn name(s: &str) -> TargetName {
    TargetName::from_latin1(s.as_bytes()).unwrap_or_else(|e| panic!("{e}"))
}

fn kept(n: &str, bytes: &[u8]) -> Target {
    let k = Kept::new(name(n), Format::Bits8, bytes.to_vec()).unwrap_or_else(|e| panic!("{e}"));
    Target { name: name(n), data: TargetData::Kept(k) }
}

fn entry(targets: Vec<Target>, class: Option<&str>) -> Entry {
    let content = Arc::new(Content::new(targets).unwrap_or_else(|e| panic!("{e}")));
    Entry::new(
        EntryId::new(7),
        content,
        Source::Local { owner_class: class.map(str::to_owned) },
        5,
        true,
    )
}

#[test]
fn a_summary_shows_kinds_size_source_and_a_clean_preview() {
    let e = entry(
        vec![
            kept("UTF8_STRING", "héllo\nworld".as_bytes()),
            kept("image/png", b"png"),
            kept("text/html", b"<b>"),
            kept("x-special/gnome-copied-files", b"copy"),
            kept("application/x-thing", b"?"),
            Target { name: name("image/bmp"), data: TargetData::NotKept(NotKept::TooLarge) },
        ],
        Some("Firefox"),
    );
    let s = summary(&e, true);
    assert_eq!((s.id, s.copied_at_ms, s.pinned, s.current), (7, 5, true, true));
    assert_eq!(
        s.kinds,
        Kinds(Kinds::TEXT | Kinds::IMAGE | Kinds::RICH_TEXT | Kinds::FILES | Kinds::OTHER)
    );
    assert_eq!(s.size, 12 + 3 + 3 + 4 + 1);
    assert_eq!(s.source.as_deref(), Some("Firefox"));
    assert_eq!(s.preview, "héllo world");
}

#[test]
fn a_hostile_source_and_huge_or_latin1_texts_are_tamed() {
    // Any client can set its WM_CLASS: escapes and bidi overrides are not passed on.
    let e = entry(vec![kept("STRING", &[b'c', 0xE9, b'\n'])], Some("\u{1b}]0;owned\u{7}\u{202e}x"));
    let s = summary(&e, false);
    assert_eq!(s.source.as_deref(), Some(" ]0;owned  x"));
    assert_eq!(s.preview, "c\u{e9} ", "Latin-1 STRING");
    let long = "€".repeat(MAX_SOURCE_LEN);
    let s = summary(&entry(vec![kept("image/png", b"p")], Some(&long)), false);
    assert!(
        s.source.as_ref().is_some_and(|c| c.len() <= MAX_SOURCE_LEN && c.chars().all(|c| c == '€'))
    );
    assert_eq!(s.preview, "", "no text, no preview");
    let huge = "x".repeat(10 << 20);
    let s = summary(&entry(vec![kept("text/plain;charset=utf-8", huge.as_bytes())], None), false);
    assert_eq!((s.preview.chars().count(), s.source), (MAX_PREVIEW_CHARS, None));
}

#[test]
fn a_preview_holds_its_characters_however_wide_and_kinds_combine() {
    let wide = "\u{1f600}".repeat(MAX_PREVIEW_CHARS + 5);
    let s = summary(
        &entry(vec![kept("UTF8_STRING", wide.as_bytes()), kept("STRING", b"x")], None),
        false,
    );
    assert_eq!(s.preview.chars().count(), MAX_PREVIEW_CHARS);
    assert_eq!(s.kinds, Kinds(Kinds::TEXT), "two text targets are one kind");
}
