//! Messages between two machines' daemons on a link's stream 0, and on a pairing session
//! (docs/link.md). Framed as the other protocols are (`[u32 BE payload length][u8 kind][payload]`),
//! but every kind has its own payload limit, checked from the 5-byte header, so a peer can never
//! make the receiver wait for, or buffer, more than the largest message of the kind it named.

use crate::{DecodeError, FRAME_HEADER_LEN, LimitError, Reader};

/// The link protocol version this code speaks; a peer's [`Message::Hello`] must carry the same.
pub const VERSION: u16 = 1;
/// The longest machine name in a [`Message::Hello`], in bytes.
pub const MAX_NAME_LEN: usize = 255;
/// The shortest interval a [`Message::Ping`] may announce, in milliseconds.
pub const MIN_PING_INTERVAL_MS: u16 = 50;
/// The longest interval a [`Message::Ping`] may announce, in milliseconds.
pub const MAX_PING_INTERVAL_MS: u16 = 10_000;
/// The length of a pairing nonce, and of its SHA-256 commitment.
pub const NONCE_LEN: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// The first message on stream 0 and on a pairing session, both ways.
    Hello {
        version: u16,
        /// Random for each start of the sending daemon: a new value means the peer restarted.
        run: u64,
        /// The sender's hostname, for display only; never trusted, never assumed printable.
        name: String,
    },
    /// Sent every `interval_ms`; the receiver suspects the session after three intervals without
    /// a message.
    Ping {
        seq: u32,
        interval_ms: u16,
    },
    Pong {
        seq: u32,
    },
    /// The sender is closing the session on purpose.
    Bye,
    /// Pairing: SHA-256 of the sender's nonce, sent before the sender has seen the peer's nonce.
    Commit {
        digest: [u8; NONCE_LEN],
    },
    /// Pairing: the nonce, sent only after the peer's [`Message::Commit`] has arrived.
    Reveal {
        nonce: [u8; NONCE_LEN],
    },
    /// Pairing: the sender's user said the codes match.
    Confirm,
}

const HELLO: u8 = 1;
const PING: u8 = 2;
const PONG: u8 = 3;
const BYE: u8 = 4;
const COMMIT: u8 = 5;
const REVEAL: u8 = 6;
const CONFIRM: u8 = 7;

/// The longest payload of each kind, or `None` for a kind this version does not know.
fn max_payload(kind: u8) -> Option<u32> {
    // A u8 length and at most 255 bytes of name: never more than 266 in all.
    const HELLO_MAX: u32 = 2 + 8 + 1 + 255;
    match kind {
        HELLO => Some(HELLO_MAX),
        PING => Some(6),
        PONG => Some(4),
        BYE | CONFIRM => Some(0),
        COMMIT | REVEAL => Some(32),
        _ => None,
    }
}

fn interval_ok(interval_ms: u16) -> Result<(), LimitError> {
    if (MIN_PING_INTERVAL_MS..=MAX_PING_INTERVAL_MS).contains(&interval_ms) {
        Ok(())
    } else {
        Err(LimitError::BadInterval { interval_ms })
    }
}

/// Appends one framed message to `out`.
///
/// # Errors
/// A [`LimitError`] if the message breaks a limit (a long name, a ping interval out of range);
/// nothing is appended then.
pub fn encode(message: &Message, out: &mut Vec<u8>) -> Result<(), LimitError> {
    let mut p = Vec::new();
    let kind = match message {
        Message::Hello { version, run, name } => {
            let len = u8::try_from(name.len())
                .map_err(|_| LimitError::TextTooLong { len: name.len() })?;
            p.extend_from_slice(&version.to_be_bytes());
            p.extend_from_slice(&run.to_be_bytes());
            p.push(len);
            p.extend_from_slice(name.as_bytes());
            HELLO
        }
        Message::Ping { seq, interval_ms } => {
            interval_ok(*interval_ms)?;
            p.extend_from_slice(&seq.to_be_bytes());
            p.extend_from_slice(&interval_ms.to_be_bytes());
            PING
        }
        Message::Pong { seq } => {
            p.extend_from_slice(&seq.to_be_bytes());
            PONG
        }
        Message::Bye => BYE,
        Message::Commit { digest } => {
            p.extend_from_slice(digest);
            COMMIT
        }
        Message::Reveal { nonce } => {
            p.extend_from_slice(nonce);
            REVEAL
        }
        Message::Confirm => CONFIRM,
    };
    // Every payload above is within its kind's limit, so the u32 conversion cannot fail.
    let len = u32::try_from(p.len()).map_err(|_| LimitError::PayloadTooLarge { len: p.len() })?;
    out.extend_from_slice(&len.to_be_bytes());
    out.push(kind);
    out.extend_from_slice(&p);
    Ok(())
}

fn u16_be(r: &mut Reader<'_>) -> Result<u16, DecodeError> {
    let pair: [u8; 2] = r.take(2)?.try_into().map_err(|_| DecodeError::Truncated)?;
    Ok(u16::from_be_bytes(pair))
}

fn bytes32(r: &mut Reader<'_>) -> Result<[u8; NONCE_LEN], DecodeError> {
    r.take(NONCE_LEN)?.try_into().map_err(|_| DecodeError::Truncated)
}

/// Decodes the first message in `buf`.
///
/// Returns `Ok(None)` when `buf` does not yet hold a whole frame, and otherwise the message and the
/// bytes it used. An unknown kind, or a length beyond its kind's limit, is refused as soon as the
/// header has arrived.
///
/// # Errors
/// A [`DecodeError`] if the frame is malformed; the session is then closed.
pub fn decode(buf: &[u8]) -> Result<Option<(Message, usize)>, DecodeError> {
    let mut r = Reader::new(buf);
    let (Ok(len), Ok(kind)) = (r.u32(), r.u8()) else { return Ok(None) };
    let max = max_payload(kind).ok_or(DecodeError::UnknownKind(kind))?;
    if len > max {
        return Err(DecodeError::FrameTooLarge { len });
    }
    let payload_len = usize::try_from(len).map_err(|_| DecodeError::FrameTooLarge { len })?;
    let Ok(payload) = r.take(payload_len) else { return Ok(None) };
    let used =
        FRAME_HEADER_LEN.checked_add(payload_len).ok_or(DecodeError::FrameTooLarge { len })?;
    let mut r = Reader::new(payload);
    let message = match kind {
        HELLO => {
            let version = u16_be(&mut r)?;
            let run = r.u64()?;
            let name_len = usize::from(r.u8()?);
            let name = String::from_utf8(r.take(name_len)?.to_vec())
                .map_err(|_| DecodeError::Limit(LimitError::TextNotUtf8))?;
            Message::Hello { version, run, name }
        }
        PING => {
            let seq = r.u32()?;
            let interval_ms = u16_be(&mut r)?;
            interval_ok(interval_ms)?;
            Message::Ping { seq, interval_ms }
        }
        PONG => Message::Pong { seq: r.u32()? },
        BYE => Message::Bye,
        COMMIT => Message::Commit { digest: bytes32(&mut r)? },
        REVEAL => Message::Reveal { nonce: bytes32(&mut r)? },
        CONFIRM => Message::Confirm,
        other => return Err(DecodeError::UnknownKind(other)),
    };
    r.finish()?;
    Ok(Some((message, used)))
}

/// What must hold of any bytes, checked by the fuzz target and by the replay of its corpus (TS-16):
/// decoding never panics; "incomplete" means the frame really is incomplete; a decoded frame needs
/// every byte it used, and re-encodes to exactly them. Returns what failed.
///
/// # Errors
/// The first property that does not hold.
#[doc(hidden)]
pub fn check_decoder(data: &[u8]) -> Result<(), String> {
    match decode(data) {
        Ok(None) => {
            let mut r = Reader::new(data);
            if let (Ok(len), Ok(_)) = (r.u32(), r.u8())
                && usize::try_from(len).is_ok_and(|len| r.take(len).is_ok())
            {
                return Err("a complete frame reported incomplete".to_owned());
            }
        }
        Ok(Some((message, used))) => {
            let mut again = Vec::new();
            encode(&message, &mut again).map_err(|e| format!("re-encoding refused: {e}"))?;
            if data.get(..used) != Some(again.as_slice()) {
                return Err("a message re-encodes differently".to_owned());
            }
            let shorter = used.checked_sub(1).and_then(|n| data.get(..n)).unwrap_or_default();
            if decode(shorter) != Ok(None) {
                return Err("a message decoded without all its bytes".to_owned());
            }
        }
        Err(_) => {}
    }
    Ok(())
}
