//! Importing clipman's saved history (docs/daemon.md, "clipman import"): its texts and images,
//! added behind everything Jackstay already has, in clipman's order.

use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use jackstay_clip::{Content, Format, Kept, Source, Target, TargetData, TargetName};
use jackstay_store::clipman;

use crate::tracker::Tracker;

/// The largest file read: far beyond what clipman keeps.
const MAX_FILE: u64 = 64 << 20;

/// What an import did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Imported {
    pub added: u32,
    /// Already held, refused, or beyond the history's limits.
    pub skipped: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportError {
    /// The texts file is there but clipman itself would not read it.
    Unreadable,
}

/// clipman's directory: `$XDG_CACHE_HOME/xfce4/clipman`.
#[must_use]
pub fn clipman_dir(cache_home: &Path) -> PathBuf {
    cache_home.join("xfce4").join("clipman")
}

fn read_capped(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?.take(MAX_FILE.saturating_add(1)).read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_FILE {
        return Err(std::io::Error::from(std::io::ErrorKind::FileTooLarge));
    }
    Ok(bytes)
}

fn modified_ms(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn single(target: &str, bytes: Vec<u8>) -> Option<Arc<Content>> {
    let name = TargetName::from_latin1(target.as_bytes()).ok()?;
    let kept = Kept::new(name.clone(), Format::Bits8, bytes).ok()?;
    Content::new(vec![Target { name, data: TargetData::Kept(kept) }]).ok().map(Arc::new)
}

/// clipman's images, `image<N>.png`, oldest (lowest N) first.
fn images(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut numbered: Vec<(u64, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let n = name.strip_prefix("image")?.strip_suffix(".png")?;
            let n: u64 = n.parse().ok().filter(|_| n.bytes().all(|b| b.is_ascii_digit()))?;
            Some((n, e.path()))
        })
        .collect();
    numbered.sort();
    numbered.into_iter().map(|(_, p)| p).collect()
}

/// Imports clipman's history from `dir` into `tracker`, behind what it holds: its texts newest
/// first, then its images newest first, so the history keeps clipman's order of each. Content
/// already held is skipped; whatever the limits do not leave room for is skipped too.
///
/// # Errors
/// [`ImportError::Unreadable`] if the texts file is there but unreadable; nothing is imported then.
pub fn import(tracker: &mut Tracker, dir: &Path, per_target: u64) -> Result<Imported, ImportError> {
    let textsrc = dir.join("textsrc");
    let texts = match read_capped(&textsrc) {
        Ok(bytes) => clipman::texts(&bytes).map_err(|_| ImportError::Unreadable)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(_) => return Err(ImportError::Unreadable),
    };
    let texts_at = modified_ms(&textsrc);
    let mut done = Imported::default();
    let source = Source::Local { owner_class: Some("xfce4-clipman".to_owned()) };
    let mut add = |content: Option<Arc<Content>>, at: u64, done: &mut Imported| {
        let added = content.is_some_and(|c| tracker.import(c, source.clone(), at));
        if added {
            done.added = done.added.saturating_add(1);
        } else {
            done.skipped = done.skipped.saturating_add(1);
        }
    };
    for text in texts.into_iter().rev() {
        add(single("UTF8_STRING", text.into_bytes()), texts_at, &mut done);
    }
    for path in images(dir).into_iter().rev() {
        let bytes = read_capped(&path)
            .ok()
            .filter(|b| u64::try_from(b.len()).is_ok_and(|l| l <= per_target));
        add(bytes.and_then(|b| single("image/png", b)), modified_ms(&path), &mut done);
    }
    Ok(done)
}

/// Deletes clipman's saved history, which would otherwise outlive every Jackstay clear.
///
/// # Errors
/// The first file that could not be deleted.
pub fn remove(dir: &Path) -> std::io::Result<()> {
    let mut first = Ok(());
    for path in std::iter::once(dir.join("textsrc")).chain(images(dir)) {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => first = first.and(Err(e)),
        }
    }
    first
}
