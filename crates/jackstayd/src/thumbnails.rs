//! Thumbnails of the history's images (docs/daemon.md, "Thumbnails"): made on a thread of their
//! own, each by a confined helper process (`jackstayd --thumbnail`, crates/thumbnail), so neither
//! the clipboard thread nor the daemon ever decodes an image. They are kept in memory only, by
//! entry, and forgotten with their entry.
//!
//! The clipboard thread says which entries want one ([`Thumbnails::sync`], after every change);
//! the thread makes them in the history's order, top first, and a result is kept only if its
//! entry still holds the content it was made from. One being made for an entry since deleted, or
//! changed, is stopped: its helper killed, and its copy of the image let go.

use std::collections::HashMap;
use std::io::{Read as _, Seek as _, Write as _};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use jackstay_clip::{Content, EntryId, History, TargetData};
use jackstay_proto::control::{Thumb, Thumbnail};
use rustix::event::{PollFd, PollFlags, Timespec, poll};

/// The most of an entry's images tried, in the order offered, before it has no thumbnail.
const MAX_TRIES: usize = 3;

/// How long one helper may take, start to finish.
const DEADLINE: Duration = Duration::from_millis(jackstay_thumbnail::HELPER_DEADLINE_MS);

/// How often a helper's wait looks whether its entry is still wanted.
const CANCEL_EVERY: Duration = Duration::from_millis(50);

/// The entries that want a thumbnail, shared with the thread, which takes the first.
#[derive(Debug, Default)]
struct Queue {
    /// In the history's order.
    wanted: Vec<(EntryId, Arc<Content>)>,
    /// The one being made.
    busy: Option<(EntryId, Arc<Content>)>,
    /// The one being made is no longer wanted.
    cancel: bool,
    stop: bool,
}

#[derive(Debug, Default)]
struct Shared {
    queue: Mutex<Queue>,
    more: Condvar,
}

impl Shared {
    fn queue(&self) -> std::sync::MutexGuard<'_, Queue> {
        // Nothing held under the lock can be left half-changed: a poisoned lock is still sound.
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn cancelled(&self) -> bool {
        let queue = self.queue();
        queue.cancel || queue.stop
    }
}

/// One thumbnail made, or none, for the content it was made from.
#[derive(Debug)]
struct Made {
    id: EntryId,
    content: Arc<Content>,
    thumbnail: Option<Thumbnail>,
}

/// The clipboard thread's side.
#[derive(Debug)]
pub struct Thumbnails {
    shared: Arc<Shared>,
    made: Receiver<Made>,
    /// Readable whenever a thumbnail is waiting.
    pub wake: UnixStream,
    /// Each entry with an image, and what it has: `Pending` until made.
    known: HashMap<EntryId, (Arc<Content>, Thumb)>,
}

/// Whether `content` holds an image.
fn has_image(content: &Content) -> bool {
    !images(content).is_empty()
}

/// The bytes of each image `content` holds, in the order offered, at most [`MAX_TRIES`].
fn images(content: &Content) -> Vec<&[u8]> {
    content
        .targets()
        .iter()
        .filter(|t| t.name.as_str().starts_with("image/"))
        .filter_map(|t| match &t.data {
            TargetData::Kept(k) => Some(k.bytes()),
            TargetData::NotKept(_) => None,
        })
        .take(MAX_TRIES)
        .collect()
}

impl Thumbnails {
    /// Starts the thread, which runs `helper --thumbnail` for each image.
    ///
    /// # Errors
    /// The thread or its pipe could not be made.
    pub fn start(helper: PathBuf) -> std::io::Result<Self> {
        let shared = Arc::new(Shared::default());
        let (made_tx, made) = mpsc::channel();
        let (wake, wake_tx) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        wake_tx.set_nonblocking(true)?;
        let theirs = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("thumbnails".to_owned())
            .spawn(move || run(&helper, &theirs, &made_tx, &wake_tx))?;
        Ok(Thumbnails { shared, made, wake, known: HashMap::new() })
    }

    /// Brings what is wanted up to date with `history`: each entry with an image that has no
    /// thumbnail of its current content is queued; entries gone are forgotten, and no longer made.
    pub fn sync(&mut self, history: &History) {
        let mut seen = HashMap::with_capacity(self.known.len());
        let mut wanted = Vec::new();
        for entry in history.entries() {
            let content = entry.content();
            if !has_image(content) {
                continue;
            }
            let id = entry.id();
            match self.known.remove(&id) {
                Some((had, thumb)) if Arc::ptr_eq(&had, content) => {
                    if thumb == Thumb::Pending {
                        wanted.push((id, Arc::clone(content)));
                    }
                    seen.insert(id, (had, thumb));
                }
                _ => {
                    wanted.push((id, Arc::clone(content)));
                    seen.insert(id, (Arc::clone(content), Thumb::Pending));
                }
            }
        }
        self.known = seen;
        let mut queue = self.shared.queue();
        let busy = queue.busy.as_ref().map(|(id, c)| (*id, Arc::clone(c)));
        if let Some((id, content)) = busy {
            let still = wanted.iter().position(|(i, c)| *i == id && Arc::ptr_eq(c, &content));
            match still {
                Some(at) => drop(wanted.remove(at)),
                None => queue.cancel = true,
            }
        }
        queue.wanted = wanted;
        drop(queue);
        self.shared.more.notify_one();
    }

    /// Takes in the thumbnails made since last asked.
    pub fn receive(&mut self) {
        let mut drained = [0u8; 64];
        while matches!((&self.wake).read(&mut drained), Ok(n) if n > 0) {}
        for made in self.made.try_iter() {
            if let Some((content, thumb)) = self.known.get_mut(&made.id)
                && Arc::ptr_eq(content, &made.content)
            {
                *thumb = made.thumbnail.map_or(Thumb::Absent, Thumb::Ready);
            }
        }
    }

    /// What there is of entry `id`'s thumbnail.
    #[must_use]
    pub fn get(&self, id: EntryId) -> Thumb {
        self.known.get(&id).map_or(Thumb::Absent, |(_, thumb)| thumb.clone())
    }
}

impl Drop for Thumbnails {
    fn drop(&mut self) {
        let mut queue = self.shared.queue();
        queue.stop = true;
        queue.wanted.clear();
        drop(queue);
        self.shared.more.notify_one();
    }
}

fn run(helper: &PathBuf, shared: &Shared, made: &Sender<Made>, wake: &UnixStream) {
    loop {
        let (id, content) = {
            let mut queue = shared.queue();
            queue.busy = None;
            queue.cancel = false;
            loop {
                if queue.stop {
                    return;
                }
                if !queue.wanted.is_empty() {
                    let first = queue.wanted.remove(0);
                    queue.busy = Some((first.0, Arc::clone(&first.1)));
                    break first;
                }
                queue = shared.more.wait(queue).unwrap_or_else(PoisonError::into_inner);
            }
        };
        let thumbnail = images(&content)
            .into_iter()
            .find_map(|bytes| if shared.cancelled() { None } else { make(helper, bytes, shared) });
        if shared.cancelled() {
            continue;
        }
        if made.send(Made { id, content, thumbnail }).is_err() {
            return;
        }
        // Full means a wake is already waiting.
        let _ = (&*wake).write(&[1]);
    }
}

/// One image through the helper: given it as a memory file, so handing it over cannot block;
/// read back within [`DEADLINE`], or the helper is killed.
fn make(helper: &PathBuf, bytes: &[u8], shared: &Shared) -> Option<Thumbnail> {
    let memory =
        rustix::fs::memfd_create("jackstay-image", rustix::fs::MemfdFlags::CLOEXEC).ok()?;
    let mut file = std::fs::File::from(memory);
    file.write_all(bytes).ok()?;
    file.rewind().ok()?;
    let mut child = Command::new(helper)
        .arg("--thumbnail")
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::from(file))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let output = read_within(&mut child, Instant::now() + DEADLINE, shared);
    let _ = child.kill();
    let status = child.wait().ok()?;
    let output = output?;
    (status.success()).then(|| Thumbnail::decode(&output).ok()).flatten()
}

/// The helper's whole output, if it ends before `deadline` and within a thumbnail's length, and
/// its entry is wanted all the while.
fn read_within(child: &mut Child, deadline: Instant, shared: &Shared) -> Option<Vec<u8>> {
    let mut stdout = child.stdout.take()?;
    let mut output = Vec::new();
    let mut chunk = vec![0u8; 64 << 10];
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        if shared.cancelled() {
            return None;
        }
        let timeout = Timespec::try_from(left.min(CANCEL_EVERY)).ok()?;
        let fd = stdout.as_fd();
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        if poll(&mut fds, Some(&timeout)).ok()? == 0 {
            continue;
        }
        let n = stdout.read(&mut chunk).ok()?;
        if n == 0 {
            return Some(output);
        }
        output.extend_from_slice(chunk.get(..n)?);
        if output.len() > Thumbnail::MAX_ENCODED_LEN {
            return None;
        }
    }
}
