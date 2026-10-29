//! Pairing (docs/link.md, "Pairing"): its own endpoint on the next port, one handshake accepted and
//! only from the named machine's addresses, then committed nonces, a code for the user to compare,
//! and both machines' confirmation.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use jackstay_proto::link::{self, Message, NONCE_LEN};
use quinn::{Connection, Incoming};
use ring::digest::{SHA256, digest};
use ring::rand::{SecureRandom as _, SystemRandom};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::{Duration, Instant, sleep_until, timeout};

use crate::identity::{Identity, PublicKey};
use crate::session::{Stream, close, peer_key};
use crate::tls::{self, Accept, PAIR_ALPN, SERVER_NAME};
use crate::{Address, PairFailure, Timings, peers};

/// The TLS exporter label for the pairing code (RFC 8446 §7.5).
const EXPORTER_LABEL: &[u8] = b"EXPORTER-jackstay-pairing";
/// How long a finished pairing waits for the other machine's goodbye, so its last `Confirm` is
/// not cut off.
const LINGER: Duration = Duration::from_secs(2);
/// The wait between dial rounds while the other machine is not yet pairing.
const REDIAL: Duration = Duration::from_millis(500);

#[derive(Debug)]
pub(crate) enum PairCommand {
    Confirm(bool),
    Cancel,
}

#[derive(Debug)]
pub(crate) enum PairOut {
    Code { name: String, fingerprint: String, code: String },
    Done(Result<(PublicKey, String), PairFailure>),
}

pub(crate) struct Args {
    pub identity: Arc<Identity>,
    /// Our main port; pairing listens on the next.
    pub port: u16,
    pub address: Address,
    pub hello: Message,
    pub timings: Timings,
    pub commands: mpsc::UnboundedReceiver<PairCommand>,
    pub out: mpsc::UnboundedSender<PairOut>,
}

pub(crate) async fn run(mut a: Args) {
    let result = pair(&mut a).await;
    let _ = a.out.send(PairOut::Done(result));
}

/// The code both machines show: SHA-256 over the exporter and both nonces, the smaller key's
/// first, as six digits.
pub(crate) fn code(
    exporter: &[u8; 32],
    first: &[u8; NONCE_LEN],
    second: &[u8; NONCE_LEN],
) -> String {
    let mut input = Vec::with_capacity(96);
    input.extend_from_slice(exporter);
    input.extend_from_slice(first);
    input.extend_from_slice(second);
    let d = digest(&SHA256, &input);
    let head = d.as_ref().first_chunk::<4>().copied().unwrap_or_default();
    format!("{:06}", u32::from_be_bytes(head) % 1_000_000)
}

fn commitment(nonce: &[u8; NONCE_LEN]) -> [u8; NONCE_LEN] {
    <[u8; NONCE_LEN]>::try_from(digest(&SHA256, nonce).as_ref()).unwrap_or([0; NONCE_LEN])
}

/// What a sub-session tells the pairing task.
enum Sub {
    Up {
        key: PublicKey,
        name: String,
        exporter: [u8; 32],
        to: mpsc::UnboundedSender<SubOut>,
    },
    Message(Message),
    /// It ended, or never came up.
    Gone,
}

enum SubOut {
    Send(Message),
    Bye,
    Close,
}

enum Start {
    Accept(Box<Incoming>),
    Dial(quinn::Endpoint, quinn::ClientConfig, Vec<SocketAddr>),
}

/// One handshake, dialed or answered: the `Hello` exchange, then messages both ways.
async fn sub(
    id: u64,
    start: Start,
    hello: Message,
    timings: Timings,
    tx: mpsc::UnboundedSender<(u64, Sub)>,
) {
    let gone = |tx: &mpsc::UnboundedSender<(u64, Sub)>| {
        let _ = tx.send((id, Sub::Gone));
    };
    let (conn, dialed) = match start {
        Start::Accept(incoming) => {
            let Ok(connecting) = (*incoming).accept() else { return gone(&tx) };
            match timeout(timings.dial_deadline, connecting).await {
                Ok(Ok(c)) => (c, false),
                _ => return gone(&tx),
            }
        }
        Start::Dial(endpoint, client, targets) => {
            let mut conn = None;
            for addr in targets {
                let Ok(connecting) = endpoint.connect_with(client.clone(), addr, SERVER_NAME)
                else {
                    continue;
                };
                if let Ok(Ok(c)) = timeout(timings.dial_deadline, connecting).await {
                    conn = Some(c);
                    break;
                }
            }
            match conn {
                Some(c) => (c, true),
                None => return gone(&tx),
            }
        }
    };
    let Some((mut stream, key, name, exporter)) = greet(&conn, dialed, &hello, &timings).await
    else {
        return gone(&tx);
    };
    let (to, mut from) = mpsc::unbounded_channel();
    let _ = tx.send((id, Sub::Up { key, name, exporter, to }));
    loop {
        tokio::select! {
            out = from.recv() => match out {
                Some(SubOut::Send(m)) => {
                    if stream.write(&m).await.is_err() {
                        return gone(&tx);
                    }
                }
                Some(SubOut::Bye) => stream.bye().await,
                Some(SubOut::Close) | None => {
                    conn.close(close::DUPLICATE, b"");
                    return gone(&tx);
                }
            },
            m = stream.read() => match m {
                Ok(m) => {
                    let _ = tx.send((id, Sub::Message(m)));
                }
                Err(_) => return gone(&tx),
            },
        }
    }
}

async fn greet(
    conn: &Connection,
    dialed: bool,
    hello: &Message,
    timings: &Timings,
) -> Option<(Stream, PublicKey, String, [u8; 32])> {
    let key = peer_key(conn)?;
    let exchange = async {
        let mut stream = Stream::open(conn, dialed).await.ok()?;
        if dialed {
            stream.write(hello).await.ok()?;
        }
        let theirs = stream.read().await.ok()?;
        if !dialed {
            stream.write(hello).await.ok()?;
        }
        Some((stream, theirs))
    };
    let (stream, theirs) = timeout(timings.dial_deadline, exchange).await.ok().flatten()?;
    let Message::Hello { version: link::VERSION, name, .. } = theirs else {
        conn.close(close::VERSION, b"");
        return None;
    };
    let mut exporter = [0u8; 32];
    conn.export_keying_material(&mut exporter, EXPORTER_LABEL, b"").ok()?;
    Some((stream, key, peers::display_name(&name), exporter))
}

/// The session pairing settled on, and where the exchange stands: each flag is one fact of it.
#[allow(clippy::struct_excessive_bools)]
struct Chosen {
    id: u64,
    key: PublicKey,
    name: String,
    exporter: [u8; 32],
    to: mpsc::UnboundedSender<SubOut>,
    theirs_committed: Option<[u8; NONCE_LEN]>,
    revealed: bool,
    code_shown: bool,
    confirmed_here: bool,
    confirmed_there: bool,
}

impl Chosen {
    fn new(
        id: u64,
        key: PublicKey,
        name: String,
        exporter: [u8; 32],
        to: mpsc::UnboundedSender<SubOut>,
    ) -> Chosen {
        Chosen {
            id,
            key,
            name,
            exporter,
            to,
            theirs_committed: None,
            revealed: false,
            code_shown: false,
            confirmed_here: false,
            confirmed_there: false,
        }
    }
}

#[allow(clippy::too_many_lines)] // One state machine, read top to bottom.
async fn pair(a: &mut Args) -> Result<(PublicKey, String), PairFailure> {
    let deadline = Instant::now() + a.timings.pairing_window;
    let own = a.identity.public();
    let peer_pair_port = a.address.port_or(a.port).checked_add(1).ok_or(PairFailure::NoAddress)?;
    let our_pair_port = a.port.checked_add(1).ok_or(PairFailure::NoAddress)?;
    let targets = a.address.resolve(peer_pair_port).await;
    if targets.is_empty() {
        return Err(PairFailure::NoAddress);
    }
    let allowed: Vec<IpAddr> = targets.iter().map(|t| t.ip().to_canonical()).collect();
    let bind = |e: std::io::Error| PairFailure::Bind(e.to_string());
    let server = tls::server_config(&a.identity, Accept::AnyBut(own), PAIR_ALPN, &a.timings)
        .map_err(bind)?;
    let client = tls::client_config(&a.identity, Accept::AnyBut(own), PAIR_ALPN, &a.timings)
        .map_err(bind)?;
    let endpoint = tls::endpoint(&a.identity, our_pair_port, Some(server)).map_err(bind)?;

    let mut nonce = [0u8; NONCE_LEN];
    SystemRandom::new().fill(&mut nonce).map_err(|_| PairFailure::Cancelled)?;
    let ours_committed = commitment(&nonce);

    let (tx, mut rx) = mpsc::unbounded_channel::<(u64, Sub)>();
    let mut tasks = JoinSet::new();
    let mut next_id: u64 = 0;
    // The one answered sub-session: its id, and whether its handshake completed. One that never
    // completes (a dial the other machine abandoned, whose closing packet still arrives as a
    // handshake) frees the place: the code depends on committed nonces exchanged only after a
    // completed handshake, so an abandoned one gains an attacker nothing.
    let mut answered: Option<(u64, bool)> = None;
    // The dialing sub-session, while there is one.
    let mut dial: Option<u64> = None;
    let mut next_dial = Instant::now();
    let mut up: HashMap<u64, (PublicKey, String, [u8; 32], mpsc::UnboundedSender<SubOut>)> =
        HashMap::new();
    let mut chosen: Option<Chosen> = None;

    let result = loop {
        // Dial until a session is up; the other machine may start pairing later than this one.
        if dial.is_none() && up.is_empty() && chosen.is_none() && Instant::now() >= next_dial {
            next_id = next_id.wrapping_add(1);
            dial = Some(next_id);
            let start = Start::Dial(endpoint.clone(), client.clone(), targets.clone());
            tasks.spawn(sub(next_id, start, a.hello.clone(), a.timings.clone(), tx.clone()));
        }
        tokio::select! {
            () = sleep_until(deadline) => break Err(PairFailure::Timeout),
            () = sleep_until(next_dial), if dial.is_none() && up.is_empty() && chosen.is_none() => {}
            command = a.commands.recv() => match command {
                Some(PairCommand::Confirm(true)) => {
                    if let Some(c) = chosen.as_mut().filter(|c| c.code_shown && !c.confirmed_here) {
                        c.confirmed_here = true;
                        let _ = c.to.send(SubOut::Send(Message::Confirm));
                    }
                }
                Some(PairCommand::Confirm(false)) => break Err(PairFailure::Declined),
                Some(PairCommand::Cancel) | None => break Err(PairFailure::Cancelled),
            },
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break Err(PairFailure::Cancelled) };
                let from = incoming.remote_address().ip().to_canonical();
                if answered.is_some() || !allowed.contains(&from) {
                    incoming.refuse();
                    break Err(PairFailure::SecondAttempt);
                }
                next_id = next_id.wrapping_add(1);
                answered = Some((next_id, false));
                tasks.spawn(sub(next_id, Start::Accept(Box::new(incoming)), a.hello.clone(), a.timings.clone(), tx.clone()));
            }
            Some(joined) = tasks.join_next() => {
                if let Err(e) = joined && e.is_panic() {
                    std::panic::resume_unwind(e.into_panic());
                }
            }
            Some((id, event)) = rx.recv() => match event {
                Sub::Up { key, name, exporter, to } => {
                    if answered == Some((id, false)) {
                        answered = Some((id, true));
                    }
                    // Every session must be with the same machine.
                    let others = up.values().map(|(k, ..)| *k).chain(chosen.as_ref().map(|c| c.key));
                    if others.into_iter().any(|k| k != key) {
                        break Err(PairFailure::SecondAttempt);
                    }
                    if chosen.is_some() {
                        let _ = to.send(SubOut::Close);
                    } else if own < key {
                        // The smaller key chooses: this session, and commits on it.
                        let _ = to.send(SubOut::Send(Message::Commit { digest: ours_committed }));
                        for (_, (.., other)) in up.drain() {
                            let _ = other.send(SubOut::Close);
                        }
                        chosen = Some(Chosen::new(id, key, name, exporter, to));
                    } else {
                        // The larger key waits for the smaller's Commit, which names the session.
                        up.insert(id, (key, name, exporter, to));
                    }
                }
                Sub::Gone => {
                    up.remove(&id);
                    if answered == Some((id, false)) {
                        answered = None;
                    }
                    if chosen.as_ref().is_some_and(|c| c.id == id) {
                        break Err(PairFailure::PeerEnded);
                    }
                    if dial == Some(id) {
                        dial = None;
                        next_dial = Instant::now() + REDIAL;
                    }
                }
                Sub::Message(m) => {
                    if chosen.is_none() {
                        let Message::Commit { .. } = m else {
                            // Before the choice, a session's first message must be the smaller
                            // key's Commit; anything else breaks the protocol. (A session already
                            // closed as a duplicate is not in `up`, and is ignored.)
                            if up.contains_key(&id) {
                                break Err(PairFailure::BadReveal);
                            }
                            continue;
                        };
                        let Some((key, name, exporter, to)) = up.remove(&id) else { continue };
                        for (_, (.., other)) in up.drain() {
                            let _ = other.send(SubOut::Close);
                        }
                        let _ = to.send(SubOut::Send(Message::Commit { digest: ours_committed }));
                        chosen = Some(Chosen::new(id, key, name, exporter, to));
                    }
                    // A session not chosen is being closed: what it says no longer matters.
                    let Some(c) = chosen.as_mut().filter(|c| c.id == id) else { continue };
                    match m {
                        Message::Commit { digest } if c.theirs_committed.is_none() => {
                            c.theirs_committed = Some(digest);
                            // Reveal only now that theirs is bound.
                            c.revealed = true;
                            let _ = c.to.send(SubOut::Send(Message::Reveal { nonce }));
                        }
                        Message::Reveal { nonce: theirs } if c.revealed && !c.code_shown => {
                            if c.theirs_committed != Some(commitment(&theirs)) {
                                break Err(PairFailure::BadReveal);
                            }
                            let (first, second) =
                                if own < c.key { (&nonce, &theirs) } else { (&theirs, &nonce) };
                            c.code_shown = true;
                            let _ = a.out.send(PairOut::Code {
                                name: c.name.clone(),
                                fingerprint: c.key.fingerprint(),
                                code: code(&c.exporter, first, second),
                            });
                        }
                        Message::Confirm if c.code_shown && !c.confirmed_there => {
                            c.confirmed_there = true;
                        }
                        Message::Bye => break Err(PairFailure::PeerEnded),
                        _ => break Err(PairFailure::BadReveal),
                    }
                }
            },
        }
        if let Some(c) = &chosen
            && c.confirmed_here
            && c.confirmed_there
        {
            break Ok((c.key, c.name.clone()));
        }
    };

    if let Some(c) = &chosen {
        let _ = c.to.send(SubOut::Bye);
        if result.is_ok() {
            // Wait for the other machine's goodbye, so the Confirm it still needs is not cut off.
            let id = c.id;
            let _ = timeout(LINGER, async {
                while let Some((from, event)) = rx.recv().await {
                    if from == id && matches!(event, Sub::Gone | Sub::Message(Message::Bye)) {
                        break;
                    }
                }
            })
            .await;
        }
    }
    endpoint.close(close::BYE, b"");
    let _ = timeout(Duration::from_millis(500), endpoint.wait_idle()).await;
    tasks.abort_all();
    result
}
