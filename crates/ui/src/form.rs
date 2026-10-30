//! The settings window's fields and the configuration they edit, without GTK (docs/ui.md,
//! "Settings"). Sizes are shown in whole MiB; a size the file gives in bytes is kept as it is
//! unless its field is changed.

use std::num::NonZeroUsize;

use jackstayd::config::{
    self, Config, ConfigError, MAX_BYTES, MAX_ENTRIES, MAX_PER_ENTRY, MAX_PER_TARGET, MAX_PINNED,
};

const MIB: u32 = 20;

/// The fields, as whole numbers: counts, and sizes in MiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fields {
    pub entries: u64,
    pub size_mib: u64,
    pub pinned: u64,
    pub target_mib: u64,
    pub entry_mib: u64,
}

/// Each field's range, (lowest, highest), in its own unit: the range the daemon accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ranges {
    pub entries: (u64, u64),
    pub size_mib: (u64, u64),
    pub pinned: (u64, u64),
    pub target_mib: (u64, u64),
    pub entry_mib: (u64, u64),
}

pub const RANGES: Ranges = Ranges {
    entries: (1, MAX_ENTRIES),
    size_mib: (0, MAX_BYTES >> MIB),
    pinned: (0, MAX_PINNED),
    target_mib: (1, MAX_PER_TARGET >> MIB),
    entry_mib: (1, MAX_PER_ENTRY >> MIB),
};

/// `bytes` in whole MiB, to the nearest, within `range`.
fn mib(bytes: u64, range: (u64, u64)) -> u64 {
    (bytes.saturating_add(1 << (MIB - 1)) >> MIB).clamp(range.0, range.1)
}

fn wide(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// The fields showing `c`.
#[must_use]
pub fn fields(c: &Config) -> Fields {
    Fields {
        entries: wide(c.max_entries.get()),
        size_mib: mib(c.max_bytes, RANGES.size_mib),
        pinned: wide(c.max_pinned),
        target_mib: mib(c.per_target, RANGES.target_mib),
        entry_mib: mib(c.per_entry, RANGES.entry_mib),
    }
}

/// The configuration `f` asks for, `was` being what the fields were filled from: a size whose
/// field is unchanged keeps its exact bytes.
///
/// # Errors
/// What the daemon would refuse, said as the window names the fields.
pub fn config(f: &Fields, was: &Config) -> Result<Config, String> {
    let checks = [
        (f.entries, RANGES.entries, "history.max_entries"),
        (f.size_mib, RANGES.size_mib, "history.max_bytes"),
        (f.pinned, RANGES.pinned, "history.max_pinned"),
        (f.target_mib, RANGES.target_mib, "capture.per_target"),
        (f.entry_mib, RANGES.entry_mib, "capture.per_entry"),
    ];
    // Each in its range first, so no size below can overflow.
    if let Some((.., key)) = checks.iter().find(|(v, (low, high), _)| !(low..=high).contains(&v)) {
        return Err(why(key));
    }
    let shown = fields(was);
    let size = |now: u64, then: u64, exact: u64| if now == then { exact } else { now << MIB };
    let c = Config {
        max_entries: usize::try_from(f.entries)
            .ok()
            .and_then(NonZeroUsize::new)
            .ok_or_else(|| why("history.max_entries"))?,
        max_bytes: size(f.size_mib, shown.size_mib, was.max_bytes),
        max_pinned: usize::try_from(f.pinned).map_err(|_| why("history.max_pinned"))?,
        per_target: size(f.target_mib, shown.target_mib, was.per_target),
        per_entry: size(f.entry_mib, shown.entry_mib, was.per_entry),
        // Not in the window: kept as the file has them.
        ..was.clone()
    };
    // The daemon's own check, so the window never saves a file the daemon would refuse. With
    // each field in its range, what is left to refuse is the one rule across two of them.
    match config::parse(&config::render(&c)) {
        Ok(back) if back == c => Ok(c),
        Err(ConfigError::BadValue(key)) if key == "capture.per_entry" => {
            Err("Most kept from one copy must be at least the largest item kept.".to_owned())
        }
        Ok(_) | Err(_) => Err(why("")),
    }
}

/// What is wrong with a key's value, as the window names it.
fn why(key: &str) -> String {
    match key {
        "history.max_entries" => "Entries kept is out of range.".to_owned(),
        "history.max_bytes" => "Size kept is out of range.".to_owned(),
        "history.max_pinned" => "Pinned entries is out of range.".to_owned(),
        "capture.per_target" => "Largest item kept is out of range.".to_owned(),
        "capture.per_entry" => "Most kept from one copy is out of range.".to_owned(),
        _ => "These settings cannot be saved.".to_owned(),
    }
}
