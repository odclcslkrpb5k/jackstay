//! Where the daemon keeps things (docs/daemon.md, "Storage", "Control socket"): resolved from the
//! XDG variables (a relative value is ignored, as the XDG spec says) or from flags, and made
//! private: ours, `0700`, never a symbolic link.

use std::ffi::OsString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{
    DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirs {
    /// `$XDG_STATE_HOME/jackstay`: the history.
    pub state: PathBuf,
    /// `$XDG_RUNTIME_DIR/jackstay`: the control socket and the instance lock.
    pub runtime: PathBuf,
    /// `$XDG_CONFIG_HOME/jackstay/config.toml`.
    pub config: PathBuf,
    /// `$XDG_CACHE_HOME`, where clipman keeps its history.
    pub cache_home: PathBuf,
}

/// Directories given on the command line, which take precedence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    pub state: Option<PathBuf>,
    pub runtime: Option<PathBuf>,
    pub config: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    /// Neither `$HOME` nor the variable that would make it unnecessary.
    NoHome,
    /// `$XDG_RUNTIME_DIR` is unset or relative: no private place for the socket.
    NoRuntimeDir,
    /// A directory that is a symbolic link, not a directory, or another user's.
    NotPrivate(PathBuf),
    Io(io::ErrorKind),
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathError::NoHome => write!(f, "HOME is not set"),
            PathError::NoRuntimeDir => write!(f, "XDG_RUNTIME_DIR is not set to an absolute path"),
            PathError::NotPrivate(p) => {
                write!(f, "{} is a link, not a directory, or not ours", p.display())
            }
            PathError::Io(kind) => write!(f, "{kind}"),
        }
    }
}

impl std::error::Error for PathError {}

/// An absolute path from the variable `name`, or `None` if it is unset, empty or relative.
fn absolute(env: &dyn Fn(&str) -> Option<OsString>, name: &str) -> Option<PathBuf> {
    env(name).map(PathBuf::from).filter(|p| p.is_absolute())
}

/// Resolves the directories from `env` (a lookup, so tests need not change the process's).
///
/// # Errors
/// [`PathError::NoRuntimeDir`] without a runtime directory; [`PathError::NoHome`] if a default
/// under `$HOME` is needed and there is none.
pub fn resolve(
    env: &dyn Fn(&str) -> Option<OsString>,
    over: &Overrides,
) -> Result<Dirs, PathError> {
    let home = || absolute(env, "HOME").ok_or(PathError::NoHome);
    let base = |var: &str, default: &str| -> Result<PathBuf, PathError> {
        match absolute(env, var) {
            Some(p) => Ok(p),
            None => Ok(home()?.join(default)),
        }
    };
    let state = match &over.state {
        Some(p) => p.clone(),
        None => base("XDG_STATE_HOME", ".local/state")?.join("jackstay"),
    };
    let runtime = match &over.runtime {
        Some(p) => p.clone(),
        None => absolute(env, "XDG_RUNTIME_DIR").ok_or(PathError::NoRuntimeDir)?.join("jackstay"),
    };
    let config = match &over.config {
        Some(p) => p.clone(),
        None => base("XDG_CONFIG_HOME", ".config")?.join("jackstay").join("config.toml"),
    };
    let cache_home = base("XDG_CACHE_HOME", ".cache")?;
    Ok(Dirs { state, runtime, config, cache_home })
}

/// Makes `dir` exist as a private directory: created `0700` if missing; if present, refused if it
/// is a symbolic link, not a directory, or another user's, and its mode tightened to `0700`.
///
/// # Errors
/// [`PathError::NotPrivate`], or the I/O error that stopped it.
pub fn private_dir(dir: &Path) -> Result<(), PathError> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| PathError::Io(e.kind()))?;
    let meta = fs::symlink_metadata(dir).map_err(|e| PathError::Io(e.kind()))?;
    // Not followed: a link is not a directory here.
    if !meta.is_dir() || meta.uid() != rustix::process::getuid().as_raw() {
        return Err(PathError::NotPrivate(dir.to_path_buf()));
    }
    if meta.permissions().mode() & 0o777 != 0o700 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| PathError::Io(e.kind()))?;
    }
    Ok(())
}

/// Takes the instance lock, `<runtime>/lock`, held for as long as the returned file is open.
/// Returns `None` if another instance holds it.
///
/// # Errors
/// The lock file cannot be opened.
pub fn lock(runtime: &Path) -> io::Result<Option<File>> {
    lock_named(runtime, "lock")
}

/// Takes the lock `<dir>/<name>`, as [`lock`] does.
///
/// # Errors
/// The lock file cannot be opened.
pub fn lock_named(dir: &Path, name: &str) -> io::Result<Option<File>> {
    let nofollow = i32::try_from(rustix::fs::OFlags::NOFOLLOW.bits()).unwrap_or(0);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(nofollow)
        .open(dir.join(name))?;
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Some(file)),
        Err(e) if e == rustix::io::Errno::WOULDBLOCK => Ok(None),
        Err(e) => Err(e.into()),
    }
}
