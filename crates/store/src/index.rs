//! The history index: one text file listing every entry, newest first, with the blob holding each
//! kept target (docs/daemon.md, "Storage").
//!
//! ```text
//! jackstay-index 2
//! next-id 44
//! tick 1761300000123
//! cleared 0
//! forgot <64 hex digits> 1761200000000 local
//! forgot <64 hex digits> 1761200000500 peer <64 hex digits>
//! entry 43 1761300000100 0 1761300000123
//! source peer <64 hex digits> xiphos
//! target UTF8_STRING kept UTF8_STRING 8 <64 hex digits>
//! target image/png remote image/png 8 33177600 <64 hex digits>
//! entry 42 1761300000000 1 1761300000000
//! source local Firefox
//! target UTF8_STRING kept UTF8_STRING 8 <64 hex digits>
//! target image/png not-kept too-large
//! end
//! ```
//!
//! Version 2 added the clock (`tick`, and each entry's last), the deletions remembered (`forgot`),
//! a peer as a source, and targets still to come from one (`remote`), for clipboard sharing
//! (docs/sharing.md). Version 1, without them, is read too: each entry's tick is its copy time.
//! A `forgot` line ends with where the deleted entry came from, `local` or `peer <key>`
//! (2026-11-13, still version 2), or with nothing when that is not known: a line written before, or
//! the deletion of an entry not held. A Jackstay from before drops such a line, alone, as
//! unreadable.
//!
//! Every field is one token in the canonical escaping of `escape.rs`. Reading salvages what it can:
//! a bad record is dropped and counted, and the rest are kept; only a bad header refuses the file.

use std::collections::HashSet;

use jackstay_clip::{EntryId, Forgotten, Format, NotKept, Origin, Source, TargetName};

use crate::BlobHash;
use crate::escape::{escape, unescape};

/// The index format this code writes; it reads this and the one before.
pub const VERSION: u64 = 2;

/// Ids and `next-id` read from disk stay below this, so a damaged file can never bring
/// [`jackstay_clip::History`] near the end of its ids, where it stops rather than reuse one.
pub const MAX_ID: u64 = 1 << 63;

const MAGIC: &[u8] = b"jackstay-index";

/// A saved history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    /// The id the next new entry will get.
    pub next_id: u64,
    /// The hybrid clock's last tick (docs/sharing.md, "Order").
    pub clock: u64,
    /// Deletions remembered, oldest first.
    pub forgotten: Vec<Forgotten>,
    /// Whether the clipboard was cleared after the newest entry was copied, so nothing is to be put
    /// back on it at start.
    pub cleared: bool,
    /// Newest first.
    pub entries: Vec<SavedEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedEntry {
    pub id: EntryId,
    /// Milliseconds since the Unix epoch, UTC.
    pub copied_at_ms: u64,
    pub pinned: bool,
    /// When it last became the clipboard, on the hybrid clock.
    pub tick: u64,
    pub source: Source,
    pub targets: Vec<SavedTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedTarget {
    pub name: TargetName,
    pub data: SavedData,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SavedData {
    Kept {
        type_name: TargetName,
        format: Format,
        blob: BlobHash,
    },
    /// Still to come from a peer: its size and the SHA-256 of its bytes.
    Remote {
        type_name: TargetName,
        format: Format,
        size: u64,
        sha256: BlobHash,
    },
    NotKept(NotKept),
}

/// What reading an index found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub index: Index,
    /// The records dropped as unreadable, numbered from 1 in file order (0 for lines before the
    /// first record), for the journal.
    pub dropped: Vec<usize>,
    /// Whether the file ended where a written index ends. If not, it was cut short (its last
    /// record, possibly part-written, is dropped) or had lines added after its end (ignored, and not
    /// counted in `dropped`).
    pub complete: bool,
    /// Whether `next-id` was raised: the header's was not above an id in the file, dropped records'
    /// included.
    pub next_id_raised: bool,
}

/// Why an index cannot be read at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    /// The file does not start as an index does.
    NotAnIndex,
    /// An index of a version this code does not know.
    UnknownVersion,
    /// The header's `next-id` or `cleared` line is unreadable.
    Malformed,
}

impl std::fmt::Display for HeaderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeaderError::NotAnIndex => write!(f, "not a jackstay index"),
            HeaderError::UnknownVersion => write!(f, "index version unknown"),
            HeaderError::Malformed => write!(f, "index header unreadable"),
        }
    }
}

impl std::error::Error for HeaderError {}

/// The index's bytes, which [`parse`] reads back to `index`.
#[must_use]
pub fn encode(index: &Index) -> Vec<u8> {
    let mut lines = vec![
        format!("{} {VERSION}", String::from_utf8_lossy(MAGIC)),
        format!("next-id {}", index.next_id),
        format!("tick {}", index.clock),
        format!("cleared {}", u8::from(index.cleared)),
    ];
    for f in &index.forgotten {
        let digest = BlobHash::from_bytes(f.digest).hex();
        lines.push(match f.origin {
            Origin::Here => format!("forgot {digest} {} local", f.tick),
            Origin::Peer(key) => {
                format!("forgot {digest} {} peer {}", f.tick, BlobHash::from_bytes(key).hex())
            }
            Origin::Unknown => format!("forgot {digest} {}", f.tick),
        });
    }
    for e in &index.entries {
        lines.push(format!(
            "entry {} {} {} {}",
            e.id.get(),
            e.copied_at_ms,
            u8::from(e.pinned),
            e.tick
        ));
        lines.push(match &e.source {
            Source::Local { owner_class: None } => "source local".to_owned(),
            Source::Local { owner_class: Some(class) } => {
                format!("source local {}", escape(class.as_bytes()))
            }
            Source::Peer { key, name } => {
                format!(
                    "source peer {} {}",
                    BlobHash::from_bytes(*key).hex(),
                    escape(name.as_bytes())
                )
            }
        });
        for t in &e.targets {
            let name = escape(&t.name.to_latin1());
            lines.push(match &t.data {
                SavedData::Kept { type_name, format, blob } => format!(
                    "target {name} kept {} {} {}",
                    escape(&type_name.to_latin1()),
                    format.bits(),
                    blob.hex()
                ),
                SavedData::Remote { type_name, format, size, sha256 } => format!(
                    "target {name} remote {} {} {size} {}",
                    escape(&type_name.to_latin1()),
                    format.bits(),
                    sha256.hex()
                ),
                SavedData::NotKept(reason) => {
                    format!("target {name} not-kept {}", reason_token(*reason))
                }
            });
        }
    }
    lines.push("end".to_owned());
    let mut out = lines.join("\n");
    out.push('\n');
    out.into_bytes()
}

fn reason_token(reason: NotKept) -> &'static str {
    match reason {
        NotKept::NotCanonical => "not-canonical",
        NotKept::TooLarge => "too-large",
        NotKept::OwnerFailed => "owner-failed",
    }
}

fn reason_of(token: &[u8]) -> Option<NotKept> {
    match token {
        b"not-canonical" => Some(NotKept::NotCanonical),
        b"too-large" => Some(NotKept::TooLarge),
        b"owner-failed" => Some(NotKept::OwnerFailed),
        _ => None,
    }
}

/// A decimal number as [`encode`] writes it: digits only, no leading zero, within `u64`.
fn number(token: &[u8]) -> Option<u64> {
    if token.is_empty() || (token.len() > 1 && token.starts_with(b"0")) {
        return None;
    }
    token.iter().try_fold(0u64, |n, &c| {
        let d = u64::from(c.checked_sub(b'0').filter(|d| *d <= 9)?);
        n.checked_mul(10)?.checked_add(d)
    })
}

fn flag(token: &[u8]) -> Option<bool> {
    match token {
        b"0" => Some(false),
        b"1" => Some(true),
        _ => None,
    }
}

fn name(token: &[u8]) -> Option<TargetName> {
    TargetName::from_latin1(&unescape(token)?).ok()
}

fn tokens(line: &[u8]) -> Vec<&[u8]> {
    line.split(|&b| b == b' ').collect()
}

/// One record being read: `None` once something in it was unreadable.
struct Record {
    number: usize,
    entry: Option<SavedEntry>,
    sourced: bool,
    /// The file's version: 1 has no ticks, peers or remote targets.
    version: u64,
}

impl Record {
    fn start(seq: usize, fields: &[&[u8]], version: u64) -> Self {
        let entry = (|| {
            let (id, at, pinned, tick) = match (fields, version) {
                ([id, at, pinned], 1) => (id, at, pinned, None),
                ([id, at, pinned, tick], 2) => (id, at, pinned, Some(number(tick)?)),
                _ => return None,
            };
            let id = number(id).filter(|&id| id < MAX_ID)?;
            let copied_at_ms = number(at)?;
            Some(SavedEntry {
                id: EntryId::new(id),
                copied_at_ms,
                pinned: flag(pinned)?,
                tick: tick.unwrap_or(copied_at_ms),
                source: Source::Local { owner_class: None },
                targets: Vec::new(),
            })
        })();
        Record { number: seq, entry, sourced: false, version }
    }

    fn source(&mut self, fields: &[&[u8]]) {
        let source = match fields {
            _ if self.sourced => None,
            [b"local"] => Some(Source::Local { owner_class: None }),
            [b"local", class] => unescape(class)
                .and_then(|c| String::from_utf8(c).ok())
                .map(|c| Source::Local { owner_class: Some(c) }),
            [b"peer", key, name] if self.version >= 2 => (|| {
                let key = BlobHash::from_hex(key)?.bytes();
                let name = String::from_utf8(unescape(name)?).ok()?;
                Some(Source::Peer { key, name })
            })(),
            _ => None,
        };
        match (source, &mut self.entry) {
            (Some(source), Some(entry)) => {
                entry.source = source;
                self.sourced = true;
            }
            _ => self.entry = None,
        }
    }

    fn target(&mut self, fields: &[&[u8]]) {
        let target = match fields {
            [n, b"kept", type_name, bits, hex] => (|| {
                let format = Format::from_bits(u8::try_from(number(bits)?).ok()?).ok()?;
                let data = SavedData::Kept {
                    type_name: name(type_name)?,
                    format,
                    blob: BlobHash::from_hex(hex)?,
                };
                Some(SavedTarget { name: name(n)?, data })
            })(),
            [n, b"remote", type_name, bits, size, hex] if self.version >= 2 => (|| {
                let format = Format::from_bits(u8::try_from(number(bits)?).ok()?).ok()?;
                let data = SavedData::Remote {
                    type_name: name(type_name)?,
                    format,
                    size: number(size)?,
                    sha256: BlobHash::from_hex(hex)?,
                };
                Some(SavedTarget { name: name(n)?, data })
            })(),
            [n, b"not-kept", reason] => (|| {
                Some(SavedTarget { name: name(n)?, data: SavedData::NotKept(reason_of(reason)?) })
            })(),
            _ => None,
        };
        match (target, &mut self.entry) {
            (Some(t), Some(entry)) => entry.targets.push(t),
            _ => self.entry = None,
        }
    }

    /// The entry, if the record is whole: a source, at least one target kept or to come, no name
    /// twice, and an id not already taken.
    fn finish(self, ids: &mut HashSet<EntryId>) -> Result<SavedEntry, usize> {
        let Some(entry) = self.entry.filter(|_| self.sourced) else { return Err(self.number) };
        let any_kept = entry
            .targets
            .iter()
            .any(|t| matches!(t.data, SavedData::Kept { .. } | SavedData::Remote { .. }));
        let mut names = HashSet::with_capacity(entry.targets.len());
        let unique = entry.targets.iter().all(|t| names.insert(&t.name));
        if any_kept && unique && ids.insert(entry.id) { Ok(entry) } else { Err(self.number) }
    }
}

/// A `forgot` line's fields: digest, tick, and the origin if known.
fn forgot(fields: &[&[u8]]) -> Option<Forgotten> {
    let (hex, tick, origin) = match fields {
        [hex, tick] => (hex, tick, Origin::Unknown),
        [hex, tick, b"local"] => (hex, tick, Origin::Here),
        [hex, tick, b"peer", key] => (hex, tick, Origin::Peer(BlobHash::from_hex(key)?.bytes())),
        _ => return None,
    };
    Some(Forgotten { digest: BlobHash::from_hex(hex)?.bytes(), tick: number(tick)?, origin })
}

fn close(
    record: Option<Record>,
    ids: &mut HashSet<EntryId>,
    entries: &mut Vec<SavedEntry>,
    dropped: &mut Vec<usize>,
) {
    if let Some(r) = record {
        match r.finish(ids) {
            Ok(entry) => entries.push(entry),
            Err(n) => dropped.push(n),
        }
    }
}

/// Reads an index, salvaging every whole record.
///
/// `next_id` is raised above every id read, so a damaged header can never let an id be reused.
///
/// # Errors
/// A [`HeaderError`] if the first lines are not an index header of this version or the one
/// before.
pub fn parse(bytes: &[u8]) -> Result<Parsed, HeaderError> {
    let (body, ends_with_newline) = match bytes.strip_suffix(b"\n") {
        Some(body) => (body, true),
        None => (bytes, false),
    };
    let mut lines = body.split(|&b| b == b'\n');
    let version = match tokens(lines.next().unwrap_or_default()).as_slice() {
        [MAGIC, version] => match number(version) {
            Some(v @ (1 | VERSION)) => v,
            _ => return Err(HeaderError::UnknownVersion),
        },
        [MAGIC, ..] => return Err(HeaderError::Malformed),
        _ => return Err(HeaderError::NotAnIndex),
    };
    let next_id = match tokens(lines.next().unwrap_or_default()).as_slice() {
        [b"next-id", n] => number(n).filter(|&n| n <= MAX_ID).ok_or(HeaderError::Malformed)?,
        _ => return Err(HeaderError::Malformed),
    };
    let clock = if version >= 2 {
        match tokens(lines.next().unwrap_or_default()).as_slice() {
            [b"tick", n] => number(n).ok_or(HeaderError::Malformed)?,
            _ => return Err(HeaderError::Malformed),
        }
    } else {
        0
    };
    let cleared = match tokens(lines.next().unwrap_or_default()).as_slice() {
        [b"cleared", c] => flag(c).ok_or(HeaderError::Malformed)?,
        _ => return Err(HeaderError::Malformed),
    };

    let mut entries = Vec::new();
    let mut dropped = Vec::new();
    let mut ids = HashSet::new();
    let mut record: Option<Record> = None;
    let mut count = 0usize;
    let mut ended = false;
    let mut trailing = false;
    // The highest id on any entry line, readable or not, so a dropped record's id is not reused.
    let mut highest: Option<u64> = None;
    let mut forgotten = Vec::new();
    for line in lines {
        if ended {
            trailing = true;
            break;
        }
        let fields = tokens(line);
        match (fields.split_first(), &mut record) {
            (Some((&b"entry", rest)), _) => {
                close(record.take(), &mut ids, &mut entries, &mut dropped);
                let id = rest.first().and_then(|id| number(id)).filter(|&id| id < MAX_ID);
                highest = highest.max(id);
                count = count.saturating_add(1);
                record = Some(Record::start(count, rest, version));
            }
            // Before the first entry; one unreadable is dropped alone.
            (Some((&b"forgot", rest)), None) if version >= 2 => match forgot(rest) {
                Some(f) => forgotten.push(f),
                None => {
                    if !dropped.contains(&0) {
                        dropped.push(0);
                    }
                }
            },
            (Some((&b"source", rest)), Some(r)) => r.source(rest),
            (Some((&b"target", rest)), Some(r)) => r.target(rest),
            (Some((&b"end", [])), _) => {
                close(record.take(), &mut ids, &mut entries, &mut dropped);
                ended = true;
            }
            (_, Some(r)) => r.entry = None,
            (_, None) => {
                if !dropped.contains(&0) {
                    dropped.push(0);
                }
            }
        }
    }
    // A file cut short may have cut its last record short too: it cannot be trusted as whole.
    if let Some(r) = record {
        dropped.push(r.number);
    }
    let above = highest.and_then(|h| h.checked_add(1)).unwrap_or(0);
    Ok(Parsed {
        index: Index { next_id: next_id.max(above), clock, forgotten, cleared, entries },
        dropped,
        complete: ended && ends_with_newline && !trailing,
        next_id_raised: above > next_id,
    })
}
