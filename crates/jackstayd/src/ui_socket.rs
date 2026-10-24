//! The UI's socket, `<runtime>/ui` (docs/ui.md, "The UI's socket"): how `jackstay popup` asks the
//! running `jackstay-ui` to show the popup. One line each way; the UI accepts only its own user.

use std::io::{self, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The socket's name in the runtime directory.
pub const NAME: &str = "ui";
/// The UI's instance lock in the runtime directory.
pub const LOCK: &str = "ui.lock";
/// Asks for the popup.
pub const POPUP: &[u8] = b"popup\n";
/// The UI's answer: it is showing it.
pub const OK: &[u8] = b"ok\n";

#[must_use]
pub fn path(runtime: &Path) -> PathBuf {
    runtime.join(NAME)
}

/// Asks the UI listening in `runtime` to show the popup, and waits up to `timeout` for its answer.
///
/// # Errors
/// The UI is not running, did not answer in time, or answered something else (`InvalidData`).
pub fn request_popup(runtime: &Path, timeout: Duration) -> io::Result<()> {
    let mut stream = UnixStream::connect(path(runtime))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(POPUP)?;
    let mut answer = Vec::new();
    stream.take(u64::try_from(OK.len()).unwrap_or(0)).read_to_end(&mut answer)?;
    if answer == OK { Ok(()) } else { Err(io::Error::from(io::ErrorKind::InvalidData)) }
}
