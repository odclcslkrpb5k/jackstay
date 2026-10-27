//! How an entry is described to clients (`jackstay list`): kinds, size, source and a one-line
//! preview of its text; never more of its contents.

use jackstay_clip::{Entry, Source, TargetData};
use jackstay_proto::control::{
    Kinds, MAX_PREVIEW_CHARS, MAX_SOURCE_LEN, Summary, preview, printable,
};

/// The kind of content a target name stands for.
fn kind_of(name: &str) -> u8 {
    match name {
        "UTF8_STRING"
        | "STRING"
        | "TEXT"
        | "COMPOUND_TEXT"
        | "text/plain"
        | "text/plain;charset=utf-8" => Kinds::TEXT,
        "x-special/gnome-copied-files" | "text/uri-list" => Kinds::FILES,
        "text/html" | "text/rtf" | "application/rtf" => Kinds::RICH_TEXT,
        n if n.starts_with("image/") => Kinds::IMAGE,
        _ => Kinds::OTHER,
    }
}

/// Enough of a text's bytes for a preview: four per character, the most UTF-8 needs.
const PREVIEW_BYTES: usize = MAX_PREVIEW_CHARS * 4;

/// The start of the text an entry holds, decoded, if any: UTF-8 targets first, then Latin-1
/// `STRING`. Only the start is decoded, so listing costs the same whatever the copies' sizes.
#[must_use]
pub fn text_start(entry: &Entry) -> Option<String> {
    let content = entry.content();
    let start = |b: &[u8]| -> Vec<u8> { b.iter().take(PREVIEW_BYTES).copied().collect() };
    ["UTF8_STRING", "text/plain;charset=utf-8"]
        .iter()
        .find_map(|n| content.kept(n))
        .map(|k| String::from_utf8_lossy(&start(k.bytes())).into_owned())
        .or_else(|| {
            content
                .kept("STRING")
                .map(|k| start(k.bytes()).iter().map(|&b| char::from(b)).collect())
        })
}

/// The longest prefix of `s` within `max` bytes, cut at a character boundary.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    let end = s
        .char_indices()
        .map(|(i, c)| i.saturating_add(c.len_utf8()))
        .take_while(|&e| e <= max)
        .last();
    s.get(..end.unwrap_or(0)).unwrap_or_default().to_owned()
}

#[must_use]
pub fn summary(entry: &Entry, current: bool) -> Summary {
    let kinds = entry
        .content()
        .targets()
        .iter()
        .filter(|t| matches!(t.data, TargetData::Kept(_)))
        .fold(0, |k, t| k | kind_of(t.name.as_str()));
    let Source::Local { owner_class } = entry.source();
    Summary {
        id: entry.id().get(),
        copied_at_ms: entry.copied_at_ms(),
        pinned: entry.pinned(),
        current,
        kinds: Kinds(kinds),
        size: entry.content().kept_len(),
        // Any client can set its WM_CLASS: it is shown only after the same cleaning as a preview.
        source: owner_class.as_deref().map(|c| truncate(&printable(c), MAX_SOURCE_LEN)),
        preview: text_start(entry).map(|t| preview(&t)).unwrap_or_default(),
    }
}
