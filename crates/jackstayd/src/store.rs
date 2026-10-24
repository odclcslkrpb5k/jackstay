//! The history on disk (docs/daemon.md, "Storage"): `index`, and `blobs/<sha256>` holding each kept
//! target's bytes once. [`load`] reads it at start; [`Store`] writes snapshots, on its own thread
//! through [`run`], so the clipboard never waits on the disk.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant};

use jackstay_clip::{Content, Entry, EntryId, History, Kept, Limits, Target, TargetData};
use jackstay_store::{BlobHash, Index, SavedData, SavedEntry, SavedTarget, encode, parse};

/// The longest index read; far beyond any history the limits allow.
const MAX_INDEX_BYTES: u64 = 256 << 20;
/// How often ordinary snapshots are written at most, and how soon a failed save is retried;
/// urgent ones go at once.
const SAVE_EVERY: Duration = Duration::from_secs(1);
/// Blobs hashed between requests while verifying at start, so a request never waits long.
const VERIFY_BATCH: usize = 8;

/// What the history is at one moment, for the store to write.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub entries: Vec<Entry>,
    pub next_id: u64,
    pub cleared: bool,
}

impl Snapshot {
    #[must_use]
    pub fn of(history: &History, cleared: bool) -> Self {
        Snapshot {
            entries: history.entries().cloned().collect(),
            next_id: history.next_id(),
            cleared,
        }
    }
}

/// Why a step on disk failed: the step and the kind of error, never a path or contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreError {
    pub step: Step,
    pub kind: io::ErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    ReadIndex,
    MoveAside,
    WriteBlob,
    WriteIndex,
    Unlink,
    ListBlobs,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?} failed: {}", self.step, self.kind)
    }
}

impl std::error::Error for StoreError {}

fn at(step: Step) -> impl Fn(io::Error) -> StoreError {
    move |e| StoreError { step, kind: e.kind() }
}

/// What [`load`] found, for the journal: counts, never contents.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadReport {
    /// Records the index could not read.
    pub dropped_records: usize,
    /// Entries whose blob was missing or did not fit its target.
    pub dropped_entries: usize,
    /// Entries the limits evicted.
    pub evicted: usize,
    /// The index was cut short or added to.
    pub incomplete: bool,
    /// The header was unreadable, so the index and blobs were moved aside.
    pub moved_aside: bool,
}

impl LoadReport {
    /// Whether everything on disk was read: only then may blobs no entry names be deleted.
    #[must_use]
    pub fn clean(&self) -> bool {
        self.dropped_records == 0
            && self.dropped_entries == 0
            && !self.incomplete
            && !self.moved_aside
    }
}

/// The saved history, read back.
#[derive(Debug)]
pub struct Loaded {
    pub history: History,
    pub cleared: bool,
    pub report: LoadReport,
    /// Blobs on disk the store may delete once unreferenced.
    present: HashSet<BlobHash>,
    /// Every blob of an entry read, with its entry, to verify after start.
    to_verify: Vec<(EntryId, BlobHash)>,
}

impl Loaded {
    /// The blobs read at start, for [`run`] to verify.
    #[must_use]
    pub fn take_loaded(&mut self) -> Vec<(EntryId, BlobHash)> {
        std::mem::take(&mut self.to_verify)
    }
}

/// Paths in the state directory.
#[derive(Debug, Clone)]
pub struct Paths {
    dir: PathBuf,
}

impl Paths {
    /// The state directory, which the caller has checked: ours, `0700`, not a symbolic link.
    #[must_use]
    pub fn new(dir: PathBuf) -> Self {
        Paths { dir }
    }

    fn index(&self) -> PathBuf {
        self.dir.join("index")
    }

    fn index_tmp(&self) -> PathBuf {
        self.dir.join("index.tmp")
    }

    fn blobs(&self) -> PathBuf {
        self.dir.join("blobs")
    }

    fn blob(&self, hash: &BlobHash) -> PathBuf {
        self.blobs().join(hash.hex())
    }

    fn blob_tmp(&self, hash: &BlobHash) -> PathBuf {
        self.blobs().join(format!(".tmp-{}", hash.hex()))
    }
}

fn nofollow() -> i32 {
    i32::try_from(rustix::fs::OFlags::NOFOLLOW.bits()).unwrap_or(0)
}

/// Opens for writing a file that must be new: never through a symbolic link, mode `0600`. A stale
/// one from an interrupted write is ours (the instance lock is held) and goes first.
fn create(path: &Path) -> io::Result<File> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(nofollow()).open(path)
}

fn read_capped(path: &Path, cap: u64) -> io::Result<Vec<u8>> {
    let file = OpenOptions::new().read(true).custom_flags(nofollow()).open(path)?;
    let mut bytes = Vec::new();
    file.take(cap.saturating_add(1)).read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > cap {
        return Err(io::Error::from(io::ErrorKind::FileTooLarge));
    }
    Ok(bytes)
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

fn make_blobs_dir(paths: &Paths) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(paths.blobs())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The blobs `blobs/` holds, by name, and the paths of anything else in it (interrupted writes).
fn list_blobs(paths: &Paths) -> Result<(HashSet<BlobHash>, Vec<PathBuf>), StoreError> {
    let mut blobs = HashSet::new();
    let mut other = Vec::new();
    for entry in fs::read_dir(paths.blobs()).map_err(at(Step::ListBlobs))? {
        let entry = entry.map_err(at(Step::ListBlobs))?;
        let name = entry.file_name();
        match name.to_str().and_then(|n| BlobHash::from_hex(n.as_bytes())) {
            Some(hash) => {
                blobs.insert(hash);
            }
            None => other.push(entry.path()),
        }
    }
    Ok((blobs, other))
}

/// Reads the saved history. A missing index is an empty history; an unreadable header moves the
/// index and the blobs aside (`*.broken-<seconds>`), deleting nothing, and starts empty; a bad
/// record or a missing blob drops that entry and keeps the rest. If the newest entry was dropped,
/// the history counts as cleared: a clear that deleted its blob may not have reached the index,
/// and nothing older may be put back on the clipboard in its place.
///
/// After a clean read, every blob no entry names (one written just before a crash, say) is
/// deleted by the first save; after any other, such blobs are kept for recovery until the history
/// is cleared.
///
/// # Errors
/// A [`StoreError`] if the directory cannot be read or set aside; the daemon then runs without
/// saving.
pub fn load(paths: &Paths, limits: Limits, per_target: u64) -> Result<Loaded, StoreError> {
    make_blobs_dir(paths).map_err(at(Step::ListBlobs))?;
    let mut report = LoadReport::default();
    let empty = |report, present| Loaded {
        history: History::new(limits),
        cleared: false,
        report,
        present,
        to_verify: Vec::new(),
    };
    let bytes = match read_capped(&paths.index(), MAX_INDEX_BYTES) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // Nothing was ever indexed: whatever blobs/ holds is left over, and may go.
            let (blobs, other) = list_blobs(paths)?;
            remove_all(&other)?;
            return Ok(empty(report, blobs));
        }
        Err(e) => return Err(at(Step::ReadIndex)(e)),
    };
    let Ok(parsed) = parse(&bytes) else {
        let suffix = format!("broken-{}", now_secs());
        fs::rename(paths.index(), paths.dir.join(format!("index.{suffix}")))
            .map_err(at(Step::MoveAside))?;
        fs::rename(paths.blobs(), paths.dir.join(format!("blobs.{suffix}")))
            .map_err(at(Step::MoveAside))?;
        make_blobs_dir(paths).map_err(at(Step::MoveAside))?;
        report.moved_aside = true;
        return Ok(empty(report, HashSet::new()));
    };
    report.dropped_records = parsed.dropped.len();
    report.incomplete = !parsed.complete;
    let mut cleared = parsed.index.cleared;
    let mut entries = Vec::new();
    let mut present = HashSet::new();
    let mut to_verify = Vec::new();
    for (i, saved) in parsed.index.entries.into_iter().enumerate() {
        let Some(entry) = read_entry(paths, &saved, per_target) else {
            report.dropped_entries = report.dropped_entries.saturating_add(1);
            cleared |= i == 0;
            continue;
        };
        for t in &saved.targets {
            if let SavedData::Kept { blob, .. } = &t.data {
                present.insert(*blob);
                to_verify.push((saved.id, *blob));
            }
        }
        entries.push(entry);
    }
    if report.clean() {
        let (blobs, other) = list_blobs(paths)?;
        remove_all(&other)?;
        present.extend(blobs);
    }
    // Parsing guarantees unique ids below next-id, so restoring cannot refuse.
    let Ok((history, evicted)) = History::restore(limits, entries, parsed.index.next_id) else {
        return Ok(empty(report, present));
    };
    report.evicted = evicted.len();
    Ok(Loaded { history, cleared, report, present, to_verify })
}

fn read_entry(paths: &Paths, saved: &SavedEntry, per_target: u64) -> Option<Entry> {
    let mut targets = Vec::with_capacity(saved.targets.len());
    for t in &saved.targets {
        let data = match &t.data {
            SavedData::Kept { type_name, format, blob } => {
                let bytes = read_capped(&paths.blob(blob), per_target).ok()?;
                TargetData::Kept(Kept::new(type_name.clone(), *format, bytes).ok()?)
            }
            SavedData::NotKept(reason) => TargetData::NotKept(*reason),
        };
        targets.push(Target { name: t.name.clone(), data });
    }
    let content = Content::new(targets).ok()?;
    let entry = Entry::new(
        saved.id,
        Arc::new(content),
        saved.source.clone(),
        saved.copied_at_ms,
        saved.pinned,
    );
    Some(entry)
}

fn remove_all(paths: &[PathBuf]) -> Result<(), StoreError> {
    let mut first = Ok(());
    for p in paths {
        if let Err(e) = remove_any(p) {
            first = first.and(Err(e));
        }
    }
    first
}

fn remove_any(path: &Path) -> Result<(), StoreError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(at(Step::Unlink)(e)),
    };
    let removed = if meta.is_dir() { fs::remove_dir_all(path) } else { fs::remove_file(path) };
    removed.map_err(at(Step::Unlink))
}

/// Writes snapshots: blobs made durable before the index names them, and deletions made durable
/// before a clear is acknowledged.
#[derive(Debug)]
pub struct Store {
    paths: Paths,
    /// Blobs on disk, possibly not yet durable: what may be deleted once unreferenced.
    present: HashSet<BlobHash>,
    /// Blobs whose directory entry is durable (`blobs/` synced since they were written): what need
    /// not be written again.
    durable: HashSet<BlobHash>,
    /// Each entry's blob names, while its content is the same `Arc`, so bytes are hashed once.
    hashes: HashMap<EntryId, (Arc<Content>, Vec<Option<BlobHash>>)>,
}

impl Store {
    #[must_use]
    pub fn new(paths: Paths, loaded: &Loaded) -> Self {
        Store {
            paths,
            present: loaded.present.clone(),
            durable: loaded.present.clone(),
            hashes: HashMap::new(),
        }
    }

    fn blob_names(&mut self, entry: &Entry) -> Vec<Option<BlobHash>> {
        if let Some((content, names)) = self.hashes.get(&entry.id())
            && Arc::ptr_eq(content, entry.content())
        {
            return names.clone();
        }
        let names: Vec<Option<BlobHash>> = entry
            .content()
            .targets()
            .iter()
            .map(|t| match &t.data {
                TargetData::Kept(k) => Some(BlobHash::of(k.bytes())),
                TargetData::NotKept(_) => None,
            })
            .collect();
        self.hashes.insert(entry.id(), (Arc::clone(entry.content()), names.clone()));
        names
    }

    /// Writes `snapshot`. If `urgent` (a clear or a deletion), unreferenced blobs are deleted, and
    /// the deletion made durable, before anything is written, so the bytes are gone even if a write
    /// then fails (a full disk, say); otherwise after the index, so the old index never names a
    /// missing blob.
    ///
    /// # Errors
    /// The first step that failed; an urgent save's deletions are attempted regardless.
    pub fn save(&mut self, snapshot: &Snapshot, urgent: bool) -> Result<(), StoreError> {
        let ids: HashSet<EntryId> = snapshot.entries.iter().map(Entry::id).collect();
        self.hashes.retain(|id, _| ids.contains(id));
        let mut saved = Vec::with_capacity(snapshot.entries.len());
        let mut needed: HashMap<BlobHash, &Kept> = HashMap::new();
        for entry in &snapshot.entries {
            let names = self.blob_names(entry);
            let mut targets = Vec::with_capacity(names.len());
            for (t, name) in entry.content().targets().iter().zip(names) {
                let data = match (&t.data, name) {
                    (TargetData::Kept(k), Some(blob)) => {
                        needed.insert(blob, k);
                        SavedData::Kept {
                            type_name: k.type_name().clone(),
                            format: k.format(),
                            blob,
                        }
                    }
                    (TargetData::NotKept(reason), _) => SavedData::NotKept(*reason),
                    (TargetData::Kept(_), None) => continue,
                };
                targets.push(SavedTarget { name: t.name.clone(), data });
            }
            saved.push(SavedEntry {
                id: entry.id(),
                copied_at_ms: entry.copied_at_ms(),
                pinned: entry.pinned(),
                source: entry.source().clone(),
                targets,
            });
        }
        let needed_names: HashSet<BlobHash> = needed.keys().copied().collect();
        if urgent {
            self.unlink_unneeded(&needed_names)?;
        }
        let mut wrote = false;
        for (blob, kept) in &needed {
            if !self.durable.contains(blob) {
                self.write_blob(blob, kept.bytes())?;
                wrote = true;
            }
        }
        if wrote {
            sync_dir(&self.paths.blobs()).map_err(at(Step::WriteBlob))?;
            self.durable.extend(needed_names.iter().copied());
        }
        let index = Index { next_id: snapshot.next_id, cleared: snapshot.cleared, entries: saved };
        self.write_index(&index)?;
        if urgent { Ok(()) } else { self.unlink_unneeded(&needed_names) }
    }

    fn write_blob(&mut self, hash: &BlobHash, bytes: &[u8]) -> Result<(), StoreError> {
        let tmp = self.paths.blob_tmp(hash);
        self.present.insert(*hash);
        let mut file = create(&tmp).map_err(at(Step::WriteBlob))?;
        file.write_all(bytes).map_err(at(Step::WriteBlob))?;
        file.sync_all().map_err(at(Step::WriteBlob))?;
        fs::rename(&tmp, self.paths.blob(hash)).map_err(at(Step::WriteBlob))
    }

    fn write_index(&self, index: &Index) -> Result<(), StoreError> {
        let tmp = self.paths.index_tmp();
        let mut file = create(&tmp).map_err(at(Step::WriteIndex))?;
        file.write_all(&encode(index)).map_err(at(Step::WriteIndex))?;
        file.sync_all().map_err(at(Step::WriteIndex))?;
        fs::rename(&tmp, self.paths.index()).map_err(at(Step::WriteIndex))?;
        sync_dir(&self.paths.dir).map_err(at(Step::WriteIndex))
    }

    /// Deletes every present blob not in `needed` (and any interrupted write of one), carrying on
    /// past failures, then makes the deletions durable. Returns the first failure.
    fn unlink_unneeded(&mut self, needed: &HashSet<BlobHash>) -> Result<(), StoreError> {
        let gone: Vec<BlobHash> = self.present.difference(needed).copied().collect();
        if gone.is_empty() {
            return Ok(());
        }
        let mut first = Ok(());
        for hash in gone {
            match remove_any(&self.paths.blob(&hash)).and(remove_any(&self.paths.blob_tmp(&hash))) {
                Ok(()) => {
                    self.present.remove(&hash);
                    self.durable.remove(&hash);
                }
                Err(e) => first = first.and(Err(e)),
            }
        }
        let synced = sync_dir(&self.paths.blobs()).map_err(at(Step::Unlink));
        first.and(synced)
    }

    /// After the history was cleared: saves, then deletes everything else the store keeps (blobs no
    /// entry names, interrupted writes, a set-aside index and blobs, a stale `index.tmp`), so
    /// nothing copied before survives on disk but what is pinned.
    ///
    /// # Errors
    /// The first step that failed; every deletion is attempted regardless.
    pub fn remove_leftovers(&mut self, snapshot: &Snapshot) -> Result<(), StoreError> {
        let saved = self.save(snapshot, true);
        let mut doomed = Vec::new();
        let (blobs, other) = list_blobs(&self.paths)?;
        doomed.extend(other);
        doomed
            .extend(blobs.iter().filter(|b| !self.present.contains(b)).map(|b| self.paths.blob(b)));
        for entry in fs::read_dir(&self.paths.dir).map_err(at(Step::ListBlobs))? {
            let entry = entry.map_err(at(Step::ListBlobs))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("index.broken-")
                || name.starts_with("blobs.broken-")
                || name == "index.tmp"
            {
                doomed.push(entry.path());
            }
        }
        let removed = remove_all(&doomed);
        let synced =
            sync_dir(&self.paths.blobs()).and(sync_dir(&self.paths.dir)).map_err(at(Step::Unlink));
        saved.and(removed).and(synced)
    }

    /// Whether the blob of `hash` on disk still matches its name.
    fn intact(&self, hash: &BlobHash, per_target: u64) -> bool {
        read_capped(&self.paths.blob(hash), per_target)
            .is_ok_and(|bytes| BlobHash::of(&bytes) == *hash)
    }

    /// Hashes every blob read at start and returns the entries whose bytes do not match their name,
    /// to be dropped.
    #[must_use]
    pub fn verify(&self, loaded: &[(EntryId, BlobHash)], per_target: u64) -> Vec<EntryId> {
        let mut bad: Vec<EntryId> = loaded
            .iter()
            .filter(|(_, hash)| !self.intact(hash, per_target))
            .map(|(id, _)| *id)
            .collect();
        bad.dedup();
        bad
    }
}

/// A request to the store thread.
#[derive(Debug)]
pub enum Request {
    /// Write this snapshot; `ack` is answered once it is on disk.
    Save { snapshot: Snapshot, urgent: bool, ack: Option<u64> },
    /// As an urgent `Save`, then delete every leftover ([`Store::remove_leftovers`]).
    ClearHistory { snapshot: Snapshot, ack: u64 },
    /// Write the latest snapshot, if any is waiting, and end.
    Stop,
}

/// What the store thread tells the clipboard thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// The request with this `ack` is done, or failed (and will be retried).
    Done { ack: u64, result: Result<(), StoreError> },
    /// A save failed; it will be retried, and the history stays usable in memory.
    Failed(StoreError),
    /// A save succeeded after failures.
    Recovered,
    /// Entries whose blobs did not match their names at start; drop them.
    Corrupt(Vec<EntryId>),
}

/// The store thread's state between requests.
struct Writer<'a> {
    store: Store,
    replies: &'a Sender<Reply>,
    wake: &'a mut std::os::unix::net::UnixStream,
    /// The latest snapshot not yet on disk: waiting out the rate limit, or after a failure.
    pending: Option<Snapshot>,
    last_save: Option<Instant>,
    failing: bool,
    /// Whether the pending snapshot is a clear-history that failed, whose leftovers must go too.
    pending_leftovers: bool,
}

impl Writer<'_> {
    fn reply(&mut self, r: Reply) {
        let _ = self.replies.send(r);
        let _ = self.wake.write_all(b"s");
    }

    /// Writes `snapshot`; on failure keeps it to retry, unless something newer is already waiting.
    fn save(
        &mut self,
        snapshot: Snapshot,
        urgent: bool,
        leftovers: bool,
    ) -> Result<(), StoreError> {
        let result = if leftovers {
            self.store.remove_leftovers(&snapshot)
        } else {
            self.store.save(&snapshot, urgent)
        };
        self.last_save = Some(Instant::now());
        if result.is_ok() {
            if std::mem::replace(&mut self.failing, false) {
                self.reply(Reply::Recovered);
            }
        } else {
            self.failing = true;
            self.pending = Some(snapshot);
            self.pending_leftovers |= leftovers;
        }
        result
    }

    /// Writes the pending snapshot; with `when_due`, only once the second since the last save has
    /// passed (the run loop's own waits already ensure that). A snapshot kept after a failure is
    /// written as urgent, so its deletions come first.
    fn save_pending(&mut self, when_due: bool) {
        if when_due && self.last_save.is_some_and(|t| t.elapsed() < SAVE_EVERY) {
            return;
        }
        let Some(snapshot) = self.pending.take() else { return };
        let leftovers = std::mem::take(&mut self.pending_leftovers);
        if let Err(e) = self.save(snapshot, self.failing, leftovers) {
            self.reply(Reply::Failed(e));
        }
    }
}

/// Runs the store thread: writes snapshots as they come, coalescing ordinary ones and writing them
/// at most once a second, retrying failed saves, and in between verifying the blobs read at start.
/// Each reply is followed by a byte on `wake`, so the clipboard thread's poll sees it.
pub fn run(
    store: Store,
    loaded: &[(EntryId, BlobHash)],
    per_target: u64,
    requests: &Receiver<Request>,
    replies: &Sender<Reply>,
    wake: &mut std::os::unix::net::UnixStream,
) {
    let mut w = Writer {
        store,
        replies,
        wake,
        pending: None,
        last_save: None,
        failing: false,
        pending_leftovers: false,
    };
    let mut to_verify: VecDeque<(EntryId, BlobHash)> = loaded.iter().copied().collect();
    let mut bad = Vec::new();
    loop {
        let next = if to_verify.is_empty() {
            let due = w.last_save.map(|t| t + SAVE_EVERY);
            match (&w.pending, due) {
                (Some(_), Some(due)) => {
                    requests.recv_timeout(due.saturating_duration_since(Instant::now()))
                }
                (Some(_), None) => Err(RecvTimeoutError::Timeout),
                (None, _) => requests.recv().map_err(|_| RecvTimeoutError::Disconnected),
            }
        } else {
            match requests.try_recv() {
                Ok(r) => Ok(r),
                Err(TryRecvError::Disconnected) => Err(RecvTimeoutError::Disconnected),
                Err(TryRecvError::Empty) => {
                    for (id, hash) in to_verify.drain(..VERIFY_BATCH.min(to_verify.len())) {
                        if !w.store.intact(&hash, per_target) && bad.last() != Some(&id) {
                            bad.push(id);
                        }
                    }
                    if to_verify.is_empty() && !bad.is_empty() {
                        w.reply(Reply::Corrupt(std::mem::take(&mut bad)));
                    }
                    w.save_pending(true);
                    continue;
                }
            }
        };
        match next {
            Ok(Request::Save { snapshot, urgent: false, ack: None }) => w.pending = Some(snapshot),
            Ok(Request::Save { snapshot, urgent, ack }) => {
                w.pending = None;
                w.pending_leftovers = false;
                let result = w.save(snapshot, urgent, false);
                match (ack, result) {
                    (Some(ack), result) => w.reply(Reply::Done { ack, result }),
                    (None, Err(e)) => w.reply(Reply::Failed(e)),
                    (None, Ok(())) => {}
                }
            }
            Ok(Request::ClearHistory { snapshot, ack }) => {
                w.pending = None;
                w.pending_leftovers = false;
                let result = w.save(snapshot, true, true);
                w.reply(Reply::Done { ack, result });
            }
            Err(RecvTimeoutError::Timeout) => w.save_pending(false),
            Ok(Request::Stop) | Err(RecvTimeoutError::Disconnected) => {
                w.save_pending(false);
                return;
            }
        }
    }
}
