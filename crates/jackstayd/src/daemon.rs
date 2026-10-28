//! Starting and stopping (docs/daemon.md, "Starting and stopping", "Exit statuses"): flags,
//! configuration, paths, the instance locks, coexistence, the store, the X connection, and the
//! restore at start; then the session until something ends it.

use std::ffi::OsString;
use std::fs::File;
use std::io::Write as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;

use jackstay_clip::{History, Limits};
use jackstay_selection::{Config as EngineConfig, Limits as CaptureLimits, Report};
use jackstay_x11_clip::{Clipboard, Error as XError};

use crate::coexist;
use crate::config::{self, Config};
use crate::control::Clients;
use crate::import;
use crate::paths::{self, Dirs, Overrides};
use crate::session::{Exit, Parts, Session, StoreLink, manager_conflict};
use crate::store::{self, Loaded, Paths, Reply, Snapshot, Store};
use crate::thumbnails::Thumbnails;
use crate::tracker::Tracker;

/// Exit statuses; systemd restarts only on 5 and 101 (`RestartPreventExitStatus=0 2 3 4`).
pub mod status {
    pub const STOPPED: i32 = 0;
    pub const BAD_SETUP: i32 = 2;
    pub const ANOTHER_MANAGER: i32 = 3;
    pub const X_LOST: i32 = 4;
    pub const X_UNREACHABLE: i32 = 5;
    pub const PANIC: i32 = 101;
}

/// Why the daemon stopped before running: an exit status and a line for the journal.
type Refusal = (i32, String);

fn log(line: &str) {
    // The journal may be gone (stderr closed); that must not stop the daemon.
    let _ = writeln!(std::io::stderr(), "{line}");
}

fn bad(why: impl std::fmt::Display) -> Refusal {
    (status::BAD_SETUP, why.to_string())
}

#[derive(Debug, Default)]
struct Flags {
    display: Option<String>,
    over: Overrides,
}

fn flags(args: &[OsString]) -> Result<Flags, String> {
    let mut f = Flags::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().cloned().ok_or(format!("{name} needs a value"));
        match arg.to_str() {
            Some("--display") => {
                let display = value("--display")?;
                f.display = Some(display.into_string().map_err(|_| "--display is not UTF-8")?);
            }
            Some("--state-dir") => f.over.state = Some(PathBuf::from(value("--state-dir")?)),
            Some("--runtime-dir") => f.over.runtime = Some(PathBuf::from(value("--runtime-dir")?)),
            Some("--config") => f.over.config = Some(PathBuf::from(value("--config")?)),
            _ => return Err(format!("unknown argument {}", arg.to_string_lossy())),
        }
    }
    Ok(f)
}

fn read_config(path: &Path) -> Result<Config, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => config::parse(&text).map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(format!("configuration unreadable: {}", e.kind())),
    }
}

/// What is set up before X is touched.
struct Setup {
    flags: Flags,
    dirs: Dirs,
    config: Config,
    /// Held while the daemon runs: one in the runtime directory, one in the state directory, so
    /// two instances share neither the socket nor the history.
    _locks: (File, File),
    clients: Clients,
    store_paths: Paths,
    loaded: Loaded,
}

/// Flags and configuration are read first (a bad one refuses before anything is touched), then
/// the runtime directory's lock is taken before anything else is made.
fn setup(args: &[OsString]) -> Result<Setup, Refusal> {
    let flags = flags(args).map_err(bad)?;
    let dirs = paths::resolve(&|name| std::env::var_os(name), &flags.over).map_err(bad)?;
    let config = read_config(&dirs.config).map_err(bad)?;
    paths::private_dir(&dirs.runtime).map_err(bad)?;
    let lock =
        paths::lock(&dirs.runtime).map_err(|e| bad(format!("instance lock: {}", e.kind())))?;
    let Some(lock) = lock else {
        return Err((status::ANOTHER_MANAGER, "another jackstayd is running".to_owned()));
    };
    if coexist::clipman_running(Path::new("/proc")) || coexist::clipman_in_panel() {
        let why = "xfce4-clipman is running; remove it from the panel first";
        return Err((status::ANOTHER_MANAGER, why.to_owned()));
    }
    paths::private_dir(&dirs.state).map_err(bad)?;
    let state_lock =
        paths::lock(&dirs.state).map_err(|e| bad(format!("state lock: {}", e.kind())))?;
    let Some(state_lock) = state_lock else {
        return Err((status::ANOTHER_MANAGER, "another jackstayd uses this history".to_owned()));
    };
    let socket = dirs.runtime.join("control");
    // Stale from a previous run: the lock is held, so no live daemon is using it.
    let _ = std::fs::remove_file(&socket);
    let clients =
        Clients::bind(&socket).map_err(|e| bad(format!("control socket: {}", e.kind())))?;
    let store_paths = Paths::new(dirs.state.clone());
    let limits = Limits { max_entries: config.max_entries, max_bytes: config.max_bytes };
    // A history that cannot be read cannot be written either, and running without one would let a
    // clear made now be undone by the next start that reads the old one: refuse instead.
    let loaded = store::load(&store_paths, limits, config.per_target)
        .map_err(|e| bad(format!("history unreadable: {e}")))?;
    report_load(&loaded);
    Ok(Setup { flags, dirs, config, _locks: (lock, state_lock), clients, store_paths, loaded })
}

fn report_load(loaded: &Loaded) {
    let r = &loaded.report;
    if !r.clean() || r.evicted > 0 {
        log(&format!(
            "history read: {} records unreadable, {} entries dropped, {} evicted{}{}",
            r.dropped_records,
            r.dropped_entries,
            r.evicted,
            if r.incomplete { ", index incomplete" } else { "" },
            if r.moved_aside { ", index moved aside" } else { "" },
        ));
    }
}

/// Connects to X, builds the tracker from the saved history, puts Jackstay's newest entry back on
/// an empty clipboard, and imports clipman's history on the first start. Returns whether it
/// imported, so the marker is written once that is saved.
fn connect(setup: &mut Setup) -> Result<(Clipboard, Tracker, bool), Refusal> {
    let config = setup.config;
    let per_target = usize::try_from(config.per_target).unwrap_or(usize::MAX);
    let per_entry = usize::try_from(config.per_entry).unwrap_or(usize::MAX);
    let engine = EngineConfig { limits: CaptureLimits { per_target, per_entry } };
    let (mut clipboard, first) = match Clipboard::connect(setup.flags.display.as_deref(), engine) {
        Ok(c) => c,
        Err(e @ (XError::Connect(_) | XError::Connection(_))) => {
            return Err((status::X_UNREACHABLE, e.to_string()));
        }
        Err(e) => return Err(bad(e)),
    };
    if let Some(Report::ManagerConflict { holder }) =
        first.iter().find(|r| matches!(r, Report::ManagerConflict { .. }))
    {
        return Err((status::ANOTHER_MANAGER, manager_conflict(holder.as_deref())));
    }
    let limits = Limits { max_entries: config.max_entries, max_bytes: config.max_bytes };
    let history = std::mem::replace(&mut setup.loaded.history, History::new(limits));
    let mut tracker = Tracker::new(history, config.max_pinned, setup.loaded.cleared);
    for r in &first {
        let _ = tracker.on_report(r, 0);
    }
    // A restart puts back Jackstay's own newest entry (never one about to be imported), only on an
    // empty clipboard, and never after a clear.
    let newest = tracker.history().newest().map(|e| (e.id(), Arc::clone(e.content())));
    if let Some((id, content)) = newest.filter(|_| !tracker.cleared()) {
        let reports =
            clipboard.restore(content).map_err(|e| (status::X_UNREACHABLE, e.to_string()))?;
        for r in &reports {
            let _ = tracker.on_report(r, 0);
        }
        tracker.served(id, reports.contains(&Report::Claimed));
    }
    let imported = first_import(setup, &mut tracker);
    Ok((clipboard, tracker, imported))
}

/// Imports clipman's history if this is the first start (no marker). Returns whether it did, even
/// if nothing was there to import, so the caller writes the marker once the history is saved.
fn first_import(setup: &Setup, tracker: &mut Tracker) -> bool {
    if marker(setup).exists() {
        return false;
    }
    let dir = import::clipman_dir(&setup.dirs.cache_home);
    let Ok(done) = import::import(tracker, &dir, setup.config.per_target) else {
        log("clipman's history is unreadable; not imported");
        return false;
    };
    if done.added > 0 {
        log(&format!(
            "imported {} entries from clipman; its files still hold them \
             (jackstay import-clipman --remove deletes them)",
            done.added
        ));
    }
    true
}

fn marker(setup: &Setup) -> PathBuf {
    setup.dirs.state.join("imported-clipman")
}

fn signal_pipe() -> Result<UnixStream, Refusal> {
    let (rx, tx) = UnixStream::pair().map_err(|e| bad(format!("signal pipe: {}", e.kind())))?;
    for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        let tx = tx.try_clone().map_err(|e| bad(format!("signal pipe: {}", e.kind())))?;
        signal_hook::low_level::pipe::register(signal, tx)
            .map_err(|e| bad(format!("signals: {}", e.kind())))?;
    }
    Ok(rx)
}

fn start_store(
    store: Store,
    to_verify: Vec<(jackstay_clip::EntryId, jackstay_store::BlobHash)>,
    per_target: u64,
) -> Result<(StoreLink, JoinHandle<()>), Refusal> {
    let (requests, requests_rx) = mpsc::channel();
    let (replies_tx, replies) = mpsc::channel();
    let (wake, mut wake_tx) =
        UnixStream::pair().map_err(|e| bad(format!("store pipe: {}", e.kind())))?;
    let thread = std::thread::Builder::new()
        .name("store".to_owned())
        .spawn(move || {
            store::run(store, &to_verify, per_target, &requests_rx, &replies_tx, &mut wake_tx);
        })
        .map_err(|e| bad(format!("store thread: {}", e.kind())))?;
    Ok((StoreLink { requests, replies, wake }, thread))
}

/// Runs the daemon with `args` (without the program name) and returns its exit status.
#[must_use]
pub fn run(args: &[OsString]) -> i32 {
    match start(args) {
        Ok(status) => status,
        Err((status, why)) => {
            log(&why);
            status
        }
    }
}

fn start(args: &[OsString]) -> Result<i32, Refusal> {
    let mut setup = setup(args)?;
    let (clipboard, tracker, imported) = connect(&mut setup)?;
    let signals = signal_pipe()?;
    let per_target = setup.config.per_target;
    let to_verify = setup.loaded.take_loaded();
    let mut store = Store::new(setup.store_paths.clone(), &setup.loaded);
    // The restore and the import changed what is to be saved; saved here, before anything else
    // runs, and the import marker written only once it is on disk, so a crash never loses an
    // import for good.
    let snapshot = Snapshot::of(tracker.history(), tracker.cleared());
    match store.save(&snapshot, false) {
        Ok(()) if imported => {
            if let Err(e) = std::fs::write(marker(&setup), b"") {
                log(&format!("import marker: {}", e.kind()));
            }
        }
        Ok(()) => {}
        Err(e) => log(&format!("store: {e}; retrying")),
    }
    let (link, store_thread) = start_store(store, to_verify, per_target)?;
    // This program itself, by the kernel's link, so an upgrade that replaces the file meanwhile
    // still runs the same helper.
    let thumbnails = match Thumbnails::start(PathBuf::from("/proc/self/exe")) {
        Ok(t) => Some(t),
        Err(e) => {
            log(&format!("thumbnails: {}; images will have none", e.kind()));
            None
        }
    };
    let mut session = Session::new(Parts {
        clipboard,
        tracker,
        clients: setup.clients,
        store: Some(link),
        signals,
        clipman_dir: import::clipman_dir(&setup.dirs.cache_home),
        per_target,
        clipman_check: Box::new(|| coexist::clipman_running(Path::new("/proc"))),
        thumbnails,
    });
    let exit = session.run();
    let status = match &exit {
        Exit::Stopped => {
            log("stopping");
            status::STOPPED
        }
        Exit::AnotherManager => {
            log("another clipboard manager took over; stopping");
            status::ANOTHER_MANAGER
        }
        Exit::Clipman => {
            log("xfce4-clipman started; stopping");
            status::ANOTHER_MANAGER
        }
        Exit::XGone(e) => {
            log(&e.to_string());
            status::X_LOST
        }
        Exit::StoreGone => {
            log("the store thread ended");
            status::PANIC
        }
    };
    // The store writes the latest snapshot before it ends; a failure then is reported.
    let link = session.store.take();
    if let Some(link) = &link {
        let _ = link.requests.send(store::Request::Stop);
    }
    if store_thread.join().is_err() {
        return Ok(status::PANIC);
    }
    if let Some(link) = link {
        for reply in link.replies.try_iter() {
            if let Reply::Failed(e) | Reply::Done { result: Err(e), .. } = reply {
                log(&format!("store: {e}; the last changes were not saved"));
            }
        }
    }
    Ok(status)
}
