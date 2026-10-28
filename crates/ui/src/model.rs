//! What the popup shows, without GTK (docs/ui.md, "The popup"): an entry as a row, the search,
//! and moving the selection.

use jackstay_proto::control::{Kinds, Summary, printable};

/// The most entries the popup asks for; the newest first.
pub const SHOWN: u32 = 100;
/// The most characters of a preview on one row.
pub const ROW_CHARS: usize = 60;

/// One entry as the popup shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: u64,
    /// The preview's start on one line, or what the entry holds when it has no text.
    pub title: String,
    /// Pinned, size, and where it was copied from.
    pub detail: String,
    /// On the clipboard now.
    pub current: bool,
    pub pinned: bool,
}

/// `entry` as a row; `image` is its image's size, once its thumbnail is known.
#[must_use]
pub fn row(entry: &Summary, image: Option<(u32, u32)>) -> Row {
    let text = one_line(&printable(&entry.preview));
    let title = if text.is_empty() {
        match image {
            Some((width, height)) if has_image(entry) => {
                format!("Image {width}×{height}")
            }
            _ => what(entry.kinds).to_owned(),
        }
    } else if text.chars().count() > ROW_CHARS {
        let cut: String = text.chars().take(ROW_CHARS.saturating_sub(1)).collect();
        format!("{}…", cut.trim_end())
    } else {
        text
    };
    let mut detail = Vec::new();
    if entry.pinned {
        detail.push("pinned".to_owned());
    }
    detail.push(size(entry.size));
    if let Some(source) = &entry.source {
        detail.push(printable(source));
    }
    Row {
        id: entry.id,
        title,
        detail: detail.join(" · "),
        current: entry.current,
        pinned: entry.pinned,
    }
}

/// Every run of whitespace as one space, trimmed.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// An entry without text, by what it holds.
fn what(kinds: Kinds) -> &'static str {
    let has = |k: u8| kinds.0 & k != 0;
    if has(Kinds::IMAGE) {
        "Image"
    } else if has(Kinds::FILES) {
        "Files"
    } else if has(Kinds::RICH_TEXT) {
        "Rich text"
    } else if has(Kinds::OTHER) {
        "Other data"
    } else {
        "Empty"
    }
}

fn size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{} KiB", bytes / 1024),
        _ => format!("{} MiB", bytes / 1_048_576),
    }
}

/// Whether `entry` matches `query`: every word of it, in any case, somewhere in the preview or
/// the source. Only the preview is searched, not text past it (docs/ui.md).
#[must_use]
pub fn matches(entry: &Summary, query: &str) -> bool {
    let haystack =
        format!("{} {}", entry.preview, entry.source.as_deref().unwrap_or_default()).to_lowercase();
    query.split_whitespace().all(|word| haystack.contains(&word.to_lowercase()))
}

/// Whether the popup asks for `entry`'s thumbnail: it holds an image.
#[must_use]
pub fn has_image(entry: &Summary) -> bool {
    entry.kinds.0 & Kinds::IMAGE != 0
}

/// The selection after moving `by` rows from `from` among `len`, kept within them.
#[must_use]
pub fn step(from: Option<usize>, by: isize, len: usize) -> Option<usize> {
    let last = len.checked_sub(1)?;
    let Some(from) = from else { return Some(0) };
    Some(from.saturating_add_signed(by).min(last))
}
