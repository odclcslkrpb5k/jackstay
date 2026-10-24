//! clipman's saved texts: `$XDG_CACHE_HOME/xfce4/clipman/textsrc`, a `GKeyFile` whose group
//! `[texts]` has a string list `texts`, oldest first (xfce4-clipman-plugin 1.6.6, plugin.c:249,
//! 279; docs/daemon.md, "clipman import").
//!
//! Read as `GLib` 2.84 loads it (`g_key_file_parse_data`, `g_key_file_parse_line`), so that what is
//! imported is what clipman would have shown, and a file clipman would refuse imports nothing:
//! - lines end at `\n`, a `\r` just before it is dropped, and a line ends at its first NUL;
//! - every line is a comment (`#`, or blank), a group (`[name]`), or a `key=value` inside a group;
//!   anything else, a key before any group, an invalid key or group name, or an `Encoding` other
//!   than UTF-8 in the first group refuses the file;
//! - the last `texts=` in any `[texts]` group wins; the list splits at each unescaped `;`, and each
//!   item's escapes (`\s \n \t \r \\ \;`) are decoded; any other escape refuses the file;
//! - empty items are left out, as clipman skips empty texts (history.c:260).

/// Why clipman's file cannot be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipmanError {
    /// A line `GLib` would refuse, so clipman would load nothing.
    NotAKeyFile,
    /// The value is not UTF-8.
    NotUtf8,
    /// An escape `GLib` does not know, or a `\` at the end.
    BadEscape,
}

impl std::fmt::Display for ClipmanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClipmanError::NotAKeyFile => write!(f, "clipman's file is not a valid key file"),
            ClipmanError::NotUtf8 => write!(f, "clipman's texts are not UTF-8"),
            ClipmanError::BadEscape => write!(f, "clipman's texts hold a bad escape"),
        }
    }
}

impl std::error::Error for ClipmanError {}

/// `GLib`'s `g_ascii_isspace`, which `g_strchug` and `g_strchomp` strip: unlike Rust's ASCII
/// whitespace, it includes vertical tab.
fn space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r')
}

fn chug(bytes: &[u8]) -> &[u8] {
    let start = bytes.iter().position(|&b| !space(b)).unwrap_or(bytes.len());
    bytes.get(start..).unwrap_or_default()
}

fn chomp(bytes: &[u8]) -> &[u8] {
    let end = bytes.iter().rposition(|&b| !space(b)).map_or(0, |i| i.saturating_add(1));
    bytes.get(..end).unwrap_or_default()
}

/// The file's lines as `g_key_file_parse_data` hands them on.
fn lines(file: &[u8]) -> impl Iterator<Item = &[u8]> {
    let (body, last_ended) = match file.strip_suffix(b"\n") {
        Some(body) => (body, true),
        None => (file, false),
    };
    let count = body.split(|&b| b == b'\n').count();
    body.split(|&b| b == b'\n').enumerate().map(move |(i, line)| {
        let ended = last_ended || i.saturating_add(1) < count;
        let line = if ended { line.strip_suffix(b"\r").unwrap_or(line) } else { line };
        line.split(|&b| b == 0).next().unwrap_or_default()
    })
}

/// `g_key_file_is_group_name`, for the text between `[` and `]`.
fn group_name(name: &[u8]) -> bool {
    !name.is_empty() && name.iter().all(|&b| b != b'[' && b != b']' && !b.is_ascii_control())
}

/// `g_key_file_is_key_name`, for a key already cut at the first `=` and stripped of space at both
/// ends: not empty, and brackets only around a locale of alphanumerics and `-_.@`.
fn key_name(key: &[u8]) -> bool {
    let base_len = key.iter().position(|&b| b == b'[' || b == b']').unwrap_or(key.len());
    let (base, rest) = key.split_at(base_len);
    if base.is_empty() {
        return false;
    }
    if rest.is_empty() {
        return true;
    }
    let Some(locale) = rest.strip_prefix(b"[").and_then(|r| r.strip_suffix(b"]")) else {
        return false;
    };
    std::str::from_utf8(locale)
        .is_ok_and(|l| l.chars().all(|c| c.is_alphanumeric() || "-_.@".contains(c)))
}

/// The texts in clipman's `textsrc`, oldest first. A file without the list holds none.
///
/// # Errors
/// A [`ClipmanError`] if `GLib` would refuse the file or the list, as clipman then imports nothing.
pub fn texts(file: &[u8]) -> Result<Vec<String>, ClipmanError> {
    let mut group: Option<&[u8]> = None;
    let mut groups = 0usize;
    let mut value: Option<&[u8]> = None;
    for line in lines(file) {
        let line = chug(line);
        if line.is_empty() || line.starts_with(b"#") {
            continue;
        }
        if let Some(inner) = line.strip_prefix(b"[")
            && let Some(close) = inner.iter().position(|&b| b == b']')
        {
            let (name, after) = inner.split_at(close);
            let after = after.get(1..).unwrap_or_default();
            if after.iter().all(|&b| b == b' ' || b == b'\t') {
                if !group_name(name) {
                    return Err(ClipmanError::NotAKeyFile);
                }
                group = Some(name);
                groups = groups.saturating_add(1);
                continue;
            }
        }
        let Some(eq) = line.iter().position(|&b| b == b'=') else {
            return Err(ClipmanError::NotAKeyFile);
        };
        let Some(current) = group else { return Err(ClipmanError::NotAKeyFile) };
        let (key, rest) = line.split_at(eq);
        let key = chomp(key);
        if !key_name(key) {
            return Err(ClipmanError::NotAKeyFile);
        }
        let v = chug(rest.get(1..).unwrap_or_default());
        if groups == 1 && key == b"Encoding" && v != b"UTF-8" {
            return Err(ClipmanError::NotAKeyFile);
        }
        if current == b"texts" && key == b"texts" {
            value = Some(v);
        }
    }
    let Some(value) = value else { return Ok(Vec::new()) };
    let value = std::str::from_utf8(value).map_err(|_| ClipmanError::NotUtf8)?;
    split(value)
}

fn split(value: &str) -> Result<Vec<String>, ClipmanError> {
    let mut items = Vec::new();
    let mut item = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => item.push(match chars.next() {
                Some('s') => ' ',
                Some('n') => '\n',
                Some('t') => '\t',
                Some('r') => '\r',
                Some('\\') => '\\',
                Some(';') => ';',
                _ => return Err(ClipmanError::BadEscape),
            }),
            ';' => items.push(std::mem::take(&mut item)),
            c => item.push(c),
        }
    }
    items.push(item);
    items.retain(|i| !i.is_empty());
    Ok(items)
}
