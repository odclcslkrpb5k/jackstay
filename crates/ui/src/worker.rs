//! The daemon, asked from a thread of its own (docs/ui.md, "Threads"): the GTK thread never waits
//! on the socket, so a stalled daemon cannot freeze the desktop's popup.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use jackstay_proto::control::{Request, Response, Summary, Thumb};
use jackstayd::client::{Client, ClientError};

use crate::model::SHOWN;

/// How long one exchange may take.
const TIMEOUT: Duration = Duration::from_secs(3);

/// What the popup asks of the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    List,
    Select(u64),
    Delete(u64),
    Pin(u64, bool),
    Clear,
    ClearHistory,
    /// An image entry's thumbnail.
    Thumbnail(u64),
}

/// What came of a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Done {
    Listed { total: u32, entries: Vec<Summary> },
    Thumbnail(u64, Thumb),
    Did(Job),
    Failed(Job, String),
}

impl Job {
    fn request(self) -> Request {
        match self {
            Job::List => Request::List { offset: 0, limit: SHOWN },
            Job::Select(id) => Request::Select { id },
            Job::Delete(id) => Request::Delete { id },
            Job::Pin(id, pinned) => Request::Pin { id, pinned },
            Job::Clear => Request::Clear,
            Job::ClearHistory => Request::ClearHistory,
            Job::Thumbnail(id) => Request::Thumbnail { id },
        }
    }
}

/// Starts the thread; each job's result is handed to `deliver`, on the worker's thread.
///
/// # Errors
/// The thread could not be started.
pub fn spawn(
    socket: PathBuf,
    deliver: impl Fn(Done) + Send + 'static,
) -> std::io::Result<mpsc::Sender<Job>> {
    let (jobs, queue) = mpsc::channel::<Job>();
    std::thread::Builder::new().name("daemon".to_owned()).spawn(move || {
        let mut client: Option<Client> = None;
        for job in queue {
            deliver(run(&socket, &mut client, job));
        }
    })?;
    Ok(jobs)
}

/// One job, reconnecting once if the connection it had has gone (the daemon restarted).
fn run(socket: &Path, client: &mut Option<Client>, job: Job) -> Done {
    let mut last = None;
    for _ in 0..2 {
        let c = match client.take() {
            Some(c) => c,
            None => match Client::connect(socket, TIMEOUT) {
                Ok(c) => c,
                Err(e) => return Done::Failed(job, why(&e)),
            },
        };
        let mut c = c;
        match c.ask(&job.request()) {
            Ok(response) => {
                *client = Some(c);
                return answer(job, response);
            }
            Err(e) => last = Some(e),
        }
    }
    Done::Failed(job, last.map_or_else(String::new, |e| why(&e)))
}

fn answer(job: Job, response: Response) -> Done {
    match (job, response) {
        (Job::List, Response::List { total, entries }) => Done::Listed { total, entries },
        (Job::Thumbnail(asked), Response::Thumbnail { id, thumbnail }) if id == asked => {
            Done::Thumbnail(id, thumbnail)
        }
        (_, Response::Error(e)) => Done::Failed(job, e.explain().to_owned()),
        (Job::List | Job::Thumbnail(_), _) => {
            Done::Failed(job, "jackstayd sent an unexpected response".to_owned())
        }
        (_, Response::Ok) => Done::Did(job),
        (_, _) => Done::Failed(job, "jackstayd sent an unexpected response".to_owned()),
    }
}

fn why(e: &ClientError) -> String {
    match e {
        ClientError::Connect(_) => "jackstayd is not running".to_owned(),
        other => other.to_string(),
    }
}
