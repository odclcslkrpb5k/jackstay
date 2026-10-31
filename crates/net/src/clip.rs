//! Clipboard sharing's streams (docs/sharing.md, "Streams"): every message on a unidirectional
//! stream of its own, every fetch on a bidirectional one, each in a task of its own so that a
//! stalled transfer never holds a session's loop, all at the lowest send priority so that stream 0
//! goes first.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use jackstay_proto::FRAME_HEADER_LEN;
use jackstay_proto::link::{self, Message};
use jackstay_proto::sharing::{Name, Sha256};
use quinn::{Connection, RecvStream, SendStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{AbortHandle, JoinSet};
use tokio::time::timeout;

use crate::identity::PublicKey;
use crate::session::{Ended, close, ended};

/// Below stream 0's `i32::MAX`: quinn sends a stream's data only when no higher one has any.
const PRIORITY: i32 = i32::MIN;
/// How long a peer's fetch waits for the daemon to find the bytes.
const ANSWER_WITHIN: Duration = Duration::from_secs(5);
/// The pieces a target is written in, so that no write holds more than this of it at once.
const PIECE: usize = 64 * 1024;
/// The largest frame on a clipboard stream: an `Announce` at its limit.
const LARGEST_FRAME: usize = FRAME_HEADER_LEN + 1024 * 1024;

/// What clipboard sharing hears from the link, through [`crate::Event::Clip`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Clip {
    /// The engine kept this session with `key`: clipboard messages go over it from now on.
    Up { key: PublicKey, session: u64 },
    /// The session kept with `key` ended.
    Down { key: PublicKey, session: u64 },
    /// An `Announce`, `Emptied` or `Forget` from the peer ([`is_shared`]).
    Received { key: PublicKey, session: u64, message: Message },
    /// The peer asks for a target: answer with [`crate::Handle::answer`] within 5 s, or it is
    /// told the target is not held.
    Wanted { key: PublicKey, request: u64, digest: Sha256, name: Name },
    /// A fetch asked with [`crate::Handle::fetch`] ended. Not reported for one cancelled.
    Fetched { fetch: u64, result: Result<Arc<[u8]>, FetchFailed> },
}

/// Why a fetch brought nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchFailed {
    /// The peer does not hold the target (any more).
    NotHeld,
    /// The session ended, or the peer stopped answering.
    Lost,
    /// The peer sent more or fewer bytes than the target's size, or something else.
    Wrong,
}

/// Whether `m` is one of the messages [`crate::Handle::share`] sends on a stream of its own.
#[must_use]
pub fn is_shared(m: &Message) -> bool {
    matches!(m, Message::Announce(_) | Message::Emptied { .. } | Message::Forget { .. })
}

/// What a session does for clipboard sharing, told by the engine.
#[derive(Debug)]
pub(crate) enum ClipOut {
    Share(Message),
    Fetch { fetch: u64, digest: Sha256, name: Name, size: u64 },
}

/// Where the daemon's answer to one [`Clip::Wanted`] goes.
type Answer = oneshot::Sender<Option<Arc<[u8]>>>;

/// What the handle and the tasks share: answers awaited from the daemon, and fetches running.
#[derive(Debug, Default)]
pub(crate) struct Shared {
    next_request: AtomicU64,
    answers: Mutex<HashMap<u64, Answer>>,
    fetches: Mutex<HashMap<u64, AbortHandle>>,
}

impl Shared {
    /// The daemon's answer to [`Clip::Wanted`].
    pub(crate) fn answer(&self, request: u64, bytes: Option<Arc<[u8]>>) {
        let waiting = self.answers.lock().unwrap_or_else(PoisonError::into_inner).remove(&request);
        if let Some(w) = waiting {
            let _ = w.send(bytes);
        }
    }

    pub(crate) fn cancel(&self, fetch: u64) {
        let running = self.fetches.lock().unwrap_or_else(PoisonError::into_inner).remove(&fetch);
        if let Some(r) = running {
            r.abort();
        }
    }
}

/// One session's clipboard streams. Its tasks end with it.
pub(crate) struct Streams {
    conn: Connection,
    key: PublicKey,
    session: u64,
    out: mpsc::UnboundedSender<Clip>,
    shared: Arc<Shared>,
    tasks: JoinSet<()>,
    /// The fetches this session started, which it reports lost if it ends first.
    fetches: Vec<u64>,
}

impl Drop for Streams {
    /// The session ended: its tasks end with it, and every fetch still running is lost.
    fn drop(&mut self) {
        let mut running = self.shared.fetches.lock().unwrap_or_else(PoisonError::into_inner);
        for fetch in self.fetches.drain(..) {
            if let Some(task) = running.remove(&fetch) {
                task.abort();
                let _ = self.out.send(Clip::Fetched { fetch, result: Err(FetchFailed::Lost) });
            }
        }
    }
}

impl Streams {
    pub(crate) fn new(
        conn: Connection,
        key: PublicKey,
        session: u64,
        out: mpsc::UnboundedSender<Clip>,
        shared: Arc<Shared>,
    ) -> Streams {
        Streams { conn, key, session, out, shared, tasks: JoinSet::new(), fetches: Vec::new() }
    }

    pub(crate) fn out(&mut self, o: ClipOut) {
        match o {
            ClipOut::Share(m) => {
                let conn = self.conn.clone();
                self.tasks.spawn(async move {
                    let mut bytes = Vec::new();
                    if !is_shared(&m) || link::encode(&m, &mut bytes).is_err() {
                        return;
                    }
                    let Ok(mut send) = conn.open_uni().await else { return };
                    let _ = send.set_priority(PRIORITY);
                    if send.write_all(&bytes).await.is_ok() {
                        let _ = send.finish();
                    }
                });
            }
            ClipOut::Fetch { fetch, digest, name, size } => {
                let (conn, out, shared) =
                    (self.conn.clone(), self.out.clone(), Arc::clone(&self.shared));
                let handle = self.tasks.spawn(async move {
                    let result = fetching(&conn, digest, name, size).await;
                    // Reported only if not cancelled meanwhile.
                    let running = shared
                        .fetches
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&fetch)
                        .is_some();
                    if running {
                        let _ = out.send(Clip::Fetched { fetch, result });
                    }
                });
                self.shared
                    .fetches
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(fetch, handle);
                // Those done (or cancelled) are forgotten here as new ones start.
                let running = self.shared.fetches.lock().unwrap_or_else(PoisonError::into_inner);
                self.fetches.retain(|f| running.contains_key(f));
                drop(running);
                self.fetches.push(fetch);
            }
        }
    }

    /// Takes the peer's clipboard streams, once the session is `kept`, and reaps their tasks,
    /// until the connection ends. Cancel-safe: run again on every turn of the session's loop.
    pub(crate) async fn serve(&mut self, kept: bool) -> Ended {
        loop {
            tokio::select! {
                // Until the session is kept, quinn holds the peer's streams, at most its limit.
                uni = self.conn.accept_uni(), if kept => match uni {
                    Ok(recv) => self.uni(recv),
                    Err(e) => return ended(&e),
                },
                bi = self.conn.accept_bi(), if kept => match bi {
                    Ok(pair) => self.bi(pair),
                    Err(e) => return ended(&e),
                },
                Some(joined) = self.tasks.join_next() => {
                    // A task's panic is not contained (docs/link.md, "In the daemon").
                    if let Err(e) = joined
                        && e.is_panic()
                    {
                        std::panic::resume_unwind(e.into_panic());
                    }
                }
                else => std::future::pending::<()>().await,
            }
        }
    }

    /// A unidirectional stream the peer opened: one message, then its end.
    pub(crate) fn uni(&mut self, mut recv: RecvStream) {
        let (conn, out, key, session) =
            (self.conn.clone(), self.out.clone(), self.key, self.session);
        self.tasks.spawn(async move {
            let message = match recv.read_to_end(LARGEST_FRAME).await {
                Ok(bytes) => whole(&bytes).filter(is_shared),
                // The session's end is the session loop's to report.
                Err(quinn::ReadToEndError::Read(quinn::ReadError::ConnectionLost(_))) => return,
                Err(_) => None,
            };
            match message {
                Some(message) => {
                    let _ = out.send(Clip::Received { key, session, message });
                }
                None => conn.close(close::PROTOCOL, b""),
            }
        });
    }

    /// A bidirectional stream the peer opened: a fetch, answered on it.
    pub(crate) fn bi(&mut self, (mut send, mut recv): (SendStream, RecvStream)) {
        let (conn, out, key, shared) =
            (self.conn.clone(), self.out.clone(), self.key, Arc::clone(&self.shared));
        self.tasks.spawn(async move {
            let _ = send.set_priority(PRIORITY);
            let asked = match recv.read_to_end(LARGEST_FRAME).await {
                Ok(bytes) => whole(&bytes),
                Err(quinn::ReadToEndError::Read(quinn::ReadError::ConnectionLost(_))) => return,
                Err(_) => None,
            };
            let Some(Message::Fetch { digest, name }) = asked else {
                conn.close(close::PROTOCOL, b"");
                return;
            };
            let request = shared.next_request.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = oneshot::channel();
            shared.answers.lock().unwrap_or_else(PoisonError::into_inner).insert(request, tx);
            let _ = out.send(Clip::Wanted { key, request, digest, name });
            let bytes = timeout(ANSWER_WITHIN, rx).await.ok().and_then(Result::ok).flatten();
            shared.answers.lock().unwrap_or_else(PoisonError::into_inner).remove(&request);
            let _ = answer(&mut send, bytes.as_deref()).await;
        });
    }
}

/// The one message `bytes` hold, using all of them.
fn whole(bytes: &[u8]) -> Option<Message> {
    match link::decode(bytes) {
        Ok(Some((m, used))) if used == bytes.len() => Some(m),
        _ => None,
    }
}

/// Writes `Answer` and, if held, the bytes in pieces, then the stream's end.
async fn answer(send: &mut SendStream, bytes: Option<&[u8]>) -> Result<(), quinn::WriteError> {
    let mut frame = Vec::new();
    let _ = link::encode(&Message::Answer { held: bytes.is_some() }, &mut frame);
    send.write_all(&frame).await?;
    for piece in bytes.unwrap_or_default().chunks(PIECE) {
        send.write_all(piece).await?;
    }
    let _ = send.finish();
    Ok(())
}

/// Asks for one target, and reads exactly `size` bytes of it, and then the stream's end.
async fn fetching(
    conn: &Connection,
    digest: Sha256,
    name: Name,
    size: u64,
) -> Result<Arc<[u8]>, FetchFailed> {
    let size = usize::try_from(size).map_err(|_| FetchFailed::Wrong)?;
    let (mut send, mut recv) = conn.open_bi().await.map_err(|_| FetchFailed::Lost)?;
    let _ = send.set_priority(PRIORITY);
    let mut frame = Vec::new();
    link::encode(&Message::Fetch { digest, name }, &mut frame).map_err(|_| FetchFailed::Wrong)?;
    send.write_all(&frame).await.map_err(|_| FetchFailed::Lost)?;
    let _ = send.finish();
    let mut head = [0u8; FRAME_HEADER_LEN + 1];
    recv.read_exact(&mut head).await.map_err(|e| read_failed(&e))?;
    match whole(&head) {
        Some(Message::Answer { held: true }) => {}
        Some(Message::Answer { held: false }) => return Err(FetchFailed::NotHeld),
        _ => return Err(FetchFailed::Wrong),
    }
    // The size is the caller's, from an announcement it already held to its limits.
    let mut bytes = vec![0u8; size];
    recv.read_exact(&mut bytes).await.map_err(|e| read_failed(&e))?;
    let mut more = [0u8; 1];
    match recv.read(&mut more).await {
        Ok(None) => Ok(bytes.into()),
        Ok(Some(_)) => Err(FetchFailed::Wrong),
        Err(_) => Err(FetchFailed::Lost),
    }
}

fn read_failed(e: &quinn::ReadExactError) -> FetchFailed {
    match e {
        // Ended short: fewer bytes than the target's size.
        quinn::ReadExactError::FinishedEarly(_) => FetchFailed::Wrong,
        quinn::ReadExactError::ReadError(_) => FetchFailed::Lost,
    }
}
