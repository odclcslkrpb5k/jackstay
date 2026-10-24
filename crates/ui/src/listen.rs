//! The UI's socket, served on a thread of its own (docs/ui.md, "The UI's socket"): `jackstay
//! popup` connects, and if it is this user's and asks for the popup, `asked` runs.

use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::Duration;

use jackstayd::{paths, ui_socket};

/// How long a client has to say what it wants.
const READ_WITHIN: Duration = Duration::from_millis(500);

/// The bound socket, and the lock that makes it this UI's.
#[derive(Debug)]
pub struct Listener {
    listener: UnixListener,
    _lock: File,
}

/// Why the UI could not listen.
#[derive(Debug)]
pub enum ListenError {
    /// Another `jackstay-ui` holds the lock.
    Running,
    Io(io::Error),
}

impl From<io::Error> for ListenError {
    fn from(e: io::Error) -> Self {
        ListenError::Io(e)
    }
}

/// Takes the UI's lock in `runtime` (made private if need be), and listens on its socket,
/// replacing one a previous UI left.
///
/// # Errors
/// [`ListenError::Running`] if another UI holds the lock; otherwise why it could not listen.
pub fn bind(runtime: &Path) -> Result<Listener, ListenError> {
    paths::private_dir(runtime).map_err(|e| io::Error::other(e.to_string()))?;
    let lock = paths::lock_named(runtime, ui_socket::LOCK)?.ok_or(ListenError::Running)?;
    let path = ui_socket::path(runtime);
    // Under the lock, a socket there is a previous UI's.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    Ok(Listener { listener, _lock: lock })
}

impl Listener {
    /// Serves forever on a new thread: each request for the popup from this user runs `asked`,
    /// and is answered once it has.
    ///
    /// # Errors
    /// The thread could not be started.
    pub fn serve(self, asked: impl Fn() + Send + 'static) -> io::Result<()> {
        let uid = rustix::process::getuid();
        let Listener { listener, _lock: lock } = self;
        std::thread::Builder::new().name("ui-socket".to_owned()).spawn(move || {
            let _held = lock;
            for stream in listener.incoming().flatten() {
                if rustix::net::sockopt::socket_peercred(&stream).is_ok_and(|c| c.uid == uid)
                    && wants_popup(&stream)
                {
                    asked();
                    let _ = (&stream).write_all(ui_socket::OK);
                }
            }
        })?;
        Ok(())
    }
}

fn wants_popup(stream: &UnixStream) -> bool {
    if stream.set_read_timeout(Some(READ_WITHIN)).is_err() {
        return false;
    }
    let mut got = Vec::new();
    let want = u64::try_from(ui_socket::POPUP.len()).unwrap_or(0);
    let _ = stream.take(want).read_to_end(&mut got);
    got == ui_socket::POPUP
}
