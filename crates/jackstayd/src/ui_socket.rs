//! The UI's socket, `<runtime>/ui` (docs/ui.md, "The UI's socket"): how `jackstay popup` and
//! `jackstay settings` ask the running `jackstay-ui` to show a window. One line each way; the UI
//! accepts only its own user.

use std::io::{self, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The socket's name in the runtime directory.
pub const NAME: &str = "ui";
/// The UI's instance lock in the runtime directory.
pub const LOCK: &str = "ui.lock";
/// The UI's answer: it is showing it.
pub const OK: &[u8] = b"ok\n";

/// What a client asks the UI to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ask {
    Popup,
    Settings,
}

impl Ask {
    /// The longest request, newline included.
    pub const LONGEST: usize = 9;

    /// The request's line.
    #[must_use]
    pub fn line(self) -> &'static [u8] {
        match self {
            Ask::Popup => b"popup\n",
            Ask::Settings => b"settings\n",
        }
    }

    /// The request a whole line is, if any.
    #[must_use]
    pub fn parse(line: &[u8]) -> Option<Ask> {
        [Ask::Popup, Ask::Settings].into_iter().find(|a| a.line() == line)
    }
}

#[must_use]
pub fn path(runtime: &Path) -> PathBuf {
    runtime.join(NAME)
}

/// Asks the UI listening in `runtime` to show what `ask` names, and waits up to `timeout` for its
/// answer.
///
/// # Errors
/// The UI is not running, did not answer in time, or answered something else (`InvalidData`).
pub fn request(runtime: &Path, ask: Ask, timeout: Duration) -> io::Result<()> {
    let mut stream = UnixStream::connect(path(runtime))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(ask.line())?;
    let mut answer = Vec::new();
    stream.take(u64::try_from(OK.len()).unwrap_or(0)).read_to_end(&mut answer)?;
    if answer == OK { Ok(()) } else { Err(io::Error::from(io::ErrorKind::InvalidData)) }
}
