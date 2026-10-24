//! Text conversions for serving aliases (docs/selection.md, "Text aliases").

/// `s` as Latin-1 (the `STRING` encoding), if every character is in Latin-1.
#[must_use]
pub fn latin1(s: &str) -> Option<Vec<u8>> {
    s.chars().map(|c| u8::try_from(c).ok()).collect()
}

/// `s` as `COMPOUND_TEXT`: Latin-1 text as is, otherwise UTF-8 inside an `ESC % G` … `ESC % @`
/// segment.
#[must_use]
pub fn compound_text(s: &str) -> Vec<u8> {
    latin1(s).unwrap_or_else(|| {
        let mut out = Vec::with_capacity(s.len().saturating_add(6));
        out.extend_from_slice(b"\x1b%G");
        out.extend_from_slice(s.as_bytes());
        out.extend_from_slice(b"\x1b%@");
        out
    })
}

/// Text decoded from a held `STRING` (Latin-1).
pub(crate) fn from_latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}
