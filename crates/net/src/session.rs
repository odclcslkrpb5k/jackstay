//! One session with a paired peer (docs/link.md, "Streams", "Liveness"): the `Hello` exchange on
//! stream 0, then pings both ways, with silence measured against the peer's announced interval;
//! and, once the engine kept it, input sharing's messages both ways (docs/input.md).

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use jackstay_proto::link::{self, Message};
use quinn::{Connection, ConnectionError, RecvStream, SendStream, TransportErrorCode};
use rustls::pki_types::CertificateDer;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until, timeout};

use crate::identity::PublicKey;
use crate::input::{Heard, Input, Sink, is_input};
use crate::{Refusal, Timings};

/// Application close codes: why a session was closed on purpose.
pub(crate) mod close {
    use quinn::VarInt;
    /// Said goodbye: shutting down, or pairing done.
    pub const BYE: VarInt = VarInt::from_u32(0);
    /// Both machines dialed; this is the session not kept.
    pub const DUPLICATE: VarInt = VarInt::from_u32(1);
    /// A newer session from the same peer took its place.
    pub const REPLACED: VarInt = VarInt::from_u32(2);
    /// The peer's `Hello` carried another version.
    pub const VERSION: VarInt = VarInt::from_u32(3);
    /// The peer sent something the protocol does not allow there.
    pub const PROTOCOL: VarInt = VarInt::from_u32(4);
    /// The peer is no longer paired.
    pub const UNPAIRED: VarInt = VarInt::from_u32(5);
}

/// TLS alerts that mean a key was refused: `bad_certificate`, `certificate_unknown`, `unknown_ca`
/// (the verifier's), `certificate_required`.
const KEY_ALERTS: [u8; 4] = [42, 46, 48, 116];
/// TLS alert `no_application_protocol`: another ALPN, so another version.
const ALPN_ALERT: u8 = 120;

/// Unanswered pings remembered for measuring round trips: at 100 ms, 1.6 s of them.
const PINGS_KEPT: usize = 16;
/// Input messages held from before the engine kept the session: the peer's engine may keep it
/// first, and send its layout at once. More than this before then is more than a peer sends, and
/// the rest are dropped.
const EARLY_KEPT: usize = 16;

/// The key the peer proved in the handshake.
pub(crate) fn peer_key(conn: &Connection) -> Option<PublicKey> {
    let any = conn.peer_identity()?;
    let certs = any.downcast::<Vec<CertificateDer<'static>>>().ok()?;
    PublicKey::from_spki(certs.first()?.as_ref())
}

/// How a session ended, as far as redialing cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ended {
    Refused(Refusal),
    /// Timed out, reset, or broken: dial again soon.
    Lost,
    /// Closed on purpose, by either side.
    Closed,
}

pub(crate) fn ended(e: &ConnectionError) -> Ended {
    let refusal = |code: TransportErrorCode| {
        if KEY_ALERTS.iter().any(|&a| code == TransportErrorCode::crypto(a)) {
            Some(Refusal::UnknownKey)
        } else if code == TransportErrorCode::crypto(ALPN_ALERT) {
            Some(Refusal::Version)
        } else {
            None
        }
    };
    match e {
        ConnectionError::ConnectionClosed(c) => {
            refusal(c.error_code).map_or(Ended::Lost, Ended::Refused)
        }
        ConnectionError::TransportError(t) => refusal(t.code).map_or(Ended::Lost, Ended::Refused),
        ConnectionError::ApplicationClosed(c) if c.error_code == close::VERSION => {
            Ended::Refused(Refusal::Version)
        }
        ConnectionError::ApplicationClosed(c) if c.error_code == close::UNPAIRED => {
            Ended::Refused(Refusal::UnknownKey)
        }
        ConnectionError::ApplicationClosed(_) | ConnectionError::LocallyClosed => Ended::Closed,
        _ => Ended::Lost,
    }
}

/// Stream 0, or a pairing session's stream: whole messages in, whole messages out.
pub(crate) struct Stream {
    send: SendStream,
    recv: RecvStream,
    buf: Vec<u8>,
}

/// Why a stream stopped.
#[derive(Debug)]
pub(crate) enum StreamError {
    Connection(ConnectionError),
    /// The peer finished or reset its side, or sent bytes that are not a message.
    Protocol,
}

impl From<quinn::WriteError> for StreamError {
    fn from(e: quinn::WriteError) -> Self {
        match e {
            quinn::WriteError::ConnectionLost(c) => StreamError::Connection(c),
            _ => StreamError::Protocol,
        }
    }
}

impl Stream {
    /// Opens the stream (dialer) or accepts it (answerer), at the highest send priority.
    pub(crate) async fn open(conn: &Connection, dialed: bool) -> Result<Stream, StreamError> {
        let (send, recv) = if dialed { conn.open_bi().await } else { conn.accept_bi().await }
            .map_err(StreamError::Connection)?;
        let _ = send.set_priority(i32::MAX);
        Ok(Stream { send, recv, buf: Vec::new() })
    }

    pub(crate) async fn write(&mut self, m: &Message) -> Result<(), StreamError> {
        let mut bytes = Vec::new();
        link::encode(m, &mut bytes).map_err(|_| StreamError::Protocol)?;
        self.send.write_all(&bytes).await?;
        Ok(())
    }

    /// The next message. Cancel-safe: bytes read stay in the buffer until a message is whole.
    pub(crate) async fn read(&mut self) -> Result<Message, StreamError> {
        loop {
            match link::decode(&self.buf) {
                Ok(Some((m, used))) => {
                    self.buf.drain(..used);
                    return Ok(m);
                }
                Ok(None) => {}
                Err(_) => return Err(StreamError::Protocol),
            }
            let mut chunk = [0u8; 512];
            match self.recv.read(&mut chunk).await {
                Ok(Some(n)) => self.buf.extend_from_slice(chunk.get(..n).unwrap_or_default()),
                Err(quinn::ReadError::ConnectionLost(c)) => return Err(StreamError::Connection(c)),
                Ok(None) | Err(_) => return Err(StreamError::Protocol),
            }
        }
    }

    /// Says goodbye: `Bye`, then the end of the stream.
    pub(crate) async fn bye(&mut self) {
        let _ = self.write(&Message::Bye).await;
        let _ = self.send.finish();
    }
}

/// The ending a stream error means.
pub(crate) fn stream_ended(conn: &Connection, e: &StreamError) -> Ended {
    match e {
        StreamError::Connection(c) => ended(c),
        StreamError::Protocol => {
            conn.close(close::PROTOCOL, b"");
            Ended::Closed
        }
    }
}

/// What the engine tells a session.
#[derive(Debug)]
pub(crate) enum ToSession {
    Active(bool),
    /// The engine kept this session: report to input sharing, and accept its messages.
    Kept,
    /// An input message to send.
    Send(Message),
    Bye,
}

/// What a session tells the engine.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Report {
    Up { key: PublicKey, run: u64 },
    Rtt(Duration),
    Suspect(bool),
    Down(Ended),
}

pub(crate) struct Args {
    pub id: u64,
    /// The peer's key, as dialed or as proven when it dialed us.
    pub key: PublicKey,
    pub conn: Connection,
    pub dialed: bool,
    /// Our `Hello`.
    pub hello: Message,
    pub active: bool,
    pub timings: Timings,
    pub reports: mpsc::UnboundedSender<(u64, Report)>,
    pub commands: mpsc::UnboundedReceiver<ToSession>,
    pub input: Sink,
}

/// What this session tells input sharing ([`Input`]): nothing until the engine keeps it, then
/// what it held from before, and from then on as it happens, ending with `Ended`.
struct ToInput {
    sink: Sink,
    key: PublicKey,
    session: u64,
    kept: bool,
    heard: Arc<Heard>,
    early: Vec<Message>,
}

impl ToInput {
    fn kept(&mut self, suspect: bool) {
        self.kept = true;
        let (key, session) = (self.key, self.session);
        (self.sink)(Input::Kept { key, session, heard: Arc::clone(&self.heard) });
        for message in self.early.drain(..) {
            (self.sink)(Input::Message { key, session, message });
        }
        if suspect {
            self.suspect(true);
        }
    }

    /// An input message from the peer. Before the engine kept the session it is held until it
    /// does; never delivered if it does not (the peer kept it alone, and it is about to close).
    fn message(&mut self, message: Message) {
        if self.kept {
            (self.sink)(Input::Message { key: self.key, session: self.session, message });
        } else if self.early.len() < EARLY_KEPT {
            self.early.push(message);
        }
    }

    fn suspect(&self, suspect: bool) {
        if self.kept {
            (self.sink)(Input::Suspect { key: self.key, session: self.session, suspect });
        }
    }

    fn ended(&self) {
        if self.kept {
            (self.sink)(Input::Ended { key: self.key, session: self.session });
        }
    }
}

fn interval_ms(d: Duration) -> u16 {
    u16::try_from(d.as_millis())
        .unwrap_or(link::MAX_PING_INTERVAL_MS)
        .clamp(link::MIN_PING_INTERVAL_MS, link::MAX_PING_INTERVAL_MS)
}

/// Runs a session until it ends, reporting as it goes. The last report is always `Down`.
pub(crate) async fn run(mut a: Args) {
    let ended = match handshake(&a).await {
        Err(e) => e,
        Ok((stream, key, run)) => {
            let _ = a.reports.send((a.id, Report::Up { key, run }));
            let mut to_input = ToInput {
                sink: Arc::clone(&a.input),
                key: a.key,
                session: a.id,
                kept: false,
                heard: Arc::new(Heard::new()),
                early: Vec::new(),
            };
            let ended = live(&mut a, stream, &mut to_input).await;
            to_input.ended();
            ended
        }
    };
    let _ = a.reports.send((a.id, Report::Down(ended)));
}

async fn handshake(a: &Args) -> Result<(Stream, PublicKey, u64), Ended> {
    let key = peer_key(&a.conn).ok_or_else(|| {
        a.conn.close(close::PROTOCOL, b"");
        Ended::Closed
    })?;
    let exchange = async {
        let mut stream = Stream::open(&a.conn, a.dialed).await?;
        // The dialer speaks first: quinn tells the peer of a stream only once it carries data.
        if a.dialed {
            stream.write(&a.hello).await?;
        }
        let theirs = stream.read().await?;
        if !a.dialed {
            stream.write(&a.hello).await?;
        }
        Ok::<_, StreamError>((stream, theirs))
    };
    let (stream, theirs) = match timeout(a.timings.dial_deadline, exchange).await {
        Err(_) => {
            a.conn.close(close::PROTOCOL, b"");
            return Err(Ended::Lost);
        }
        Ok(Err(e)) => return Err(stream_ended(&a.conn, &e)),
        Ok(Ok(x)) => x,
    };
    match theirs {
        Message::Hello { version, run, .. } if version == link::VERSION => Ok((stream, key, run)),
        Message::Hello { .. } => {
            a.conn.close(close::VERSION, b"");
            Err(Ended::Refused(Refusal::Version))
        }
        _ => {
            a.conn.close(close::PROTOCOL, b"");
            Err(Ended::Closed)
        }
    }
}

async fn live(a: &mut Args, mut stream: Stream, to_input: &mut ToInput) -> Ended {
    let report = {
        let (reports, id) = (a.reports.clone(), a.id);
        move |r: Report| {
            let _ = reports.send((id, r));
        }
    };
    let mut interval = if a.active { a.timings.active_ping } else { a.timings.idle_ping };
    let mut seq: u32 = 0;
    // Pings not yet answered, oldest first: a round trip longer than the interval is still
    // measured.
    let mut sent: VecDeque<(u32, Instant)> = VecDeque::with_capacity(PINGS_KEPT);
    let mut next_ping = Instant::now();
    let mut heard = Instant::now();
    // Until the peer's first ping says otherwise, expect its idle interval.
    let mut theirs = a.timings.idle_ping;
    let mut suspect = false;
    loop {
        // Silent for three of the peer's intervals; or, sooner when we are the faster pinger, our
        // oldest ping since we last heard anything unanswered for three of ours.
        let theirs_due = heard + theirs * 3;
        let ours_due = sent.iter().find(|(_, at)| *at > heard).map(|(_, at)| *at + interval * 3);
        let suspect_at = ours_due.map_or(theirs_due, |o| o.min(theirs_due));
        tokio::select! {
            biased;
            command = a.commands.recv() => match command {
                Some(ToSession::Active(on)) => {
                    let wanted = if on { a.timings.active_ping } else { a.timings.idle_ping };
                    if wanted != interval {
                        interval = wanted;
                        next_ping = Instant::now();
                    }
                }
                Some(ToSession::Kept) => to_input.kept(suspect),
                Some(ToSession::Send(m)) => {
                    if let Err(e) = stream.write(&m).await {
                        return stream_ended(&a.conn, &e);
                    }
                }
                Some(ToSession::Bye) | None => {
                    stream.bye().await;
                    a.conn.close(close::BYE, b"");
                    return Ended::Closed;
                }
            },
            message = stream.read() => {
                let message = match message {
                    Ok(m) => m,
                    Err(e) => return stream_ended(&a.conn, &e),
                };
                heard = Instant::now();
                to_input.heard.stamp();
                if suspect {
                    suspect = false;
                    report(Report::Suspect(false));
                    to_input.suspect(false);
                }
                match message {
                    Message::Ping { seq, interval_ms } => {
                        theirs = Duration::from_millis(u64::from(interval_ms));
                        if let Err(e) = stream.write(&Message::Pong { seq }).await {
                            return stream_ended(&a.conn, &e);
                        }
                    }
                    Message::Pong { seq: answered } => {
                        if let Some(i) = sent.iter().position(|(s, _)| *s == answered) {
                            if let Some((_, at)) = sent.get(i) {
                                report(Report::Rtt(at.elapsed()));
                            }
                            // This one and every older one are done with.
                            sent.drain(..=i);
                        }
                    }
                    Message::Bye => {
                        a.conn.close(close::BYE, b"");
                        return Ended::Closed;
                    }
                    message if is_input(&message) => to_input.message(message),
                    _ => {
                        a.conn.close(close::PROTOCOL, b"");
                        return Ended::Closed;
                    }
                }
            }
            () = sleep_until(next_ping) => {
                seq = seq.wrapping_add(1);
                let ping = Message::Ping { seq, interval_ms: interval_ms(interval) };
                if let Err(e) = stream.write(&ping).await {
                    return stream_ended(&a.conn, &e);
                }
                if sent.len() >= PINGS_KEPT {
                    sent.pop_front();
                }
                sent.push_back((seq, Instant::now()));
                next_ping = Instant::now() + interval;
            }
            () = sleep_until(suspect_at), if !suspect => {
                suspect = true;
                report(Report::Suspect(true));
                to_input.suspect(true);
            }
            e = a.conn.closed() => return ended(&e),
        }
    }
}
