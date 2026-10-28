//! What the popup shows, without a display.

use jackstay_proto::control::{Kinds, Summary};
use jackstay_ui::model::{ROW_CHARS, Row, matches, row, step};

fn summary(preview: &str, kinds: u8, size: u64, source: Option<&str>) -> Summary {
    Summary {
        id: 9,
        copied_at_ms: 0,
        pinned: false,
        current: false,
        kinds: Kinds(kinds),
        size,
        source: source.map(str::to_owned),
        preview: preview.to_owned(),
    }
}

#[test]
fn a_row_is_one_clean_line_with_its_size_and_source() {
    let mut s = summary("  two\n\tlines \u{202e}x ", Kinds::TEXT, 1536, Some("Fire\u{1b}fox"));
    s.pinned = true;
    s.current = true;
    assert_eq!(
        row(&s, None),
        Row {
            id: 9,
            title: "two lines x".to_owned(),
            detail: "pinned · 1 KiB · Fire fox".to_owned(),
            current: true,
            pinned: true,
        }
    );
    assert_eq!(row(&summary("a", Kinds::TEXT, 1023, None), None).detail, "1023 B");
    assert_eq!(row(&summary("a", Kinds::TEXT, 3 << 20, None), None).detail, "3 MiB");
    assert_eq!(row(&summary("a", Kinds::TEXT, 1 << 20, None), None).detail, "1 MiB");
    assert_eq!(row(&summary("a", Kinds::TEXT, 1024, None), None).detail, "1 KiB");
}

#[test]
fn a_long_preview_is_cut_and_one_without_text_says_what_it_holds() {
    let exact = "x".repeat(ROW_CHARS);
    assert_eq!(row(&summary(&exact, Kinds::TEXT, 1, None), None).title, exact);
    let long = format!("{}y", "é".repeat(ROW_CHARS));
    let title = row(&summary(&long, Kinds::TEXT, 1, None), None).title;
    assert_eq!(title.chars().count(), ROW_CHARS);
    assert!(title.ends_with('…'));
    // Trailing space before the cut is not kept.
    let spaced = format!("{} {}", "a".repeat(ROW_CHARS - 2), "b".repeat(10));
    assert_eq!(
        row(&summary(&spaced, Kinds::TEXT, 1, None), None).title,
        format!("{}…", "a".repeat(ROW_CHARS - 2))
    );
    for (kinds, what) in [
        (Kinds::IMAGE | Kinds::OTHER, "Image"),
        (Kinds::FILES | Kinds::RICH_TEXT, "Files"),
        (Kinds::RICH_TEXT, "Rich text"),
        (Kinds::OTHER, "Other data"),
        (0, "Empty"),
    ] {
        assert_eq!(row(&summary(" \n", kinds, 1, None), None).title, what);
    }
}

#[test]
fn search_needs_every_word_in_any_case_in_the_preview_or_source() {
    let s = summary("Hello World", Kinds::TEXT, 1, Some("Mousepad"));
    for query in ["", "  ", "hello", "WORLD hello", "mouse", "world pad"] {
        assert!(matches(&s, query), "{query:?}");
    }
    for query in ["hello there", "firefox", "helloworld"] {
        assert!(!matches(&s, query), "{query:?}");
    }
    assert!(matches(&summary("É", Kinds::TEXT, 1, None), "é"));
}

#[test]
fn moving_stays_within_the_rows() {
    assert_eq!(step(None, 1, 0), None);
    assert_eq!(step(Some(0), 1, 0), None);
    assert_eq!(step(None, 1, 3), Some(0));
    assert_eq!(step(Some(0), 1, 3), Some(1));
    assert_eq!(step(Some(2), 1, 3), Some(2));
    assert_eq!(step(Some(1), -5, 3), Some(0));
    assert_eq!(step(Some(1), 10, 3), Some(2));
}

#[test]
fn an_image_is_titled_with_its_size_once_known() {
    let image = summary("", Kinds::IMAGE, 5000, None);
    assert_eq!(row(&image, None).title, "Image");
    assert_eq!(row(&image, Some((1920, 1080))).title, "Image 1920×1080");
    // A caption is kept as the title.
    let captioned = summary("a cat", Kinds::IMAGE | Kinds::TEXT, 5000, None);
    assert_eq!(row(&captioned, Some((1920, 1080))).title, "a cat");
    // Without an image, a size is not shown.
    assert_eq!(row(&summary("", Kinds::FILES, 5, None), Some((1, 1))).title, "Files");
    assert!(jackstay_ui::model::has_image(&image));
    assert!(!jackstay_ui::model::has_image(&summary("a", Kinds::TEXT, 1, None)));
}
