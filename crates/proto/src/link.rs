//! Messages between two machines' daemons on a link's stream 0, and on a pairing session
//! (docs/link.md), and input sharing's on stream 0 (docs/input.md, "Messages"). Framed as the
//! other protocols are (`[u32 BE payload length][u8 kind][payload]`), but every kind has its own
//! payload limit, checked from the 5-byte header, so a peer can never make the receiver wait for,
//! or buffer, more than the largest message of the kind it named.

use crate::{DecodeError, FRAME_HEADER_LEN, LimitError, Reader};

/// The link protocol version this code speaks; a peer's [`Message::Hello`] must carry the same.
/// 2 added input sharing's kinds; 3 added [`Message::Entered`].
pub const VERSION: u16 = 3;
/// The longest machine name in a [`Message::Hello`], in bytes.
pub const MAX_NAME_LEN: usize = 255;
/// The shortest interval a [`Message::Ping`] may announce, in milliseconds.
pub const MIN_PING_INTERVAL_MS: u16 = 50;
/// The longest interval a [`Message::Ping`] may announce, in milliseconds.
pub const MAX_PING_INTERVAL_MS: u16 = 10_000;
/// The length of a pairing nonce, and of its SHA-256 commitment.
pub const NONCE_LEN: usize = 32;
/// The most monitors in a [`Message::Layout`].
pub const MAX_MONITORS: usize = 16;
/// The largest coordinate a monitor may reach: X's root window is at most 32767 pixels a side.
pub const MAX_COORDINATE: u16 = 32_767;
/// The lowest keycode X uses.
pub const MIN_KEYCODE: u8 = 8;
/// The most keycodes in a [`Message::Keymap`]: every one from [`MIN_KEYCODE`] to 255.
pub const MAX_KEYMAP_KEYS: usize = 248;
/// The highest mouse button forwarded (4 to 7 are the wheel).
pub const MAX_BUTTON: u8 = 9;
/// The largest [`Message::Motion`] delta, in 1/256 px: 65,536 pixels.
pub const MAX_MOTION: i32 = 1 << 24;
/// [`Message::Enter`]'s lock bits.
pub const LOCK_CAPS: u8 = 1;
pub const LOCK_NUM: u8 = 2;

/// A side of a machine's monitors' bounding box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    pub const ALL: [Side; 4] = [Side::Left, Side::Right, Side::Top, Side::Bottom];

    /// The side a pointer leaving through this one enters by.
    #[must_use]
    pub fn opposite(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
            Side::Top => Side::Bottom,
            Side::Bottom => Side::Top,
        }
    }

    fn code(self) -> u8 {
        match self {
            Side::Left => 0,
            Side::Right => 1,
            Side::Top => 2,
            Side::Bottom => 3,
        }
    }

    fn from_code(code: u8) -> Result<Side, LimitError> {
        match code {
            0 => Ok(Side::Left),
            1 => Ok(Side::Right),
            2 => Ok(Side::Top),
            3 => Ok(Side::Bottom),
            _ => Err(LimitError::UnknownCode { code }),
        }
    }
}

/// One monitor, in root-window pixels: never empty, never beyond [`MAX_COORDINATE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Monitor {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Monitor {
    /// # Errors
    /// [`LimitError::BadMonitor`] if it is empty or reaches beyond [`MAX_COORDINATE`].
    pub fn check(&self) -> Result<(), LimitError> {
        let fits = |at: u16, len: u16| {
            len > 0 && u32::from(at).saturating_add(u32::from(len)) <= u32::from(MAX_COORDINATE) + 1
        };
        if fits(self.x, self.width) && fits(self.y, self.height) {
            Ok(())
        } else {
            Err(LimitError::BadMonitor)
        }
    }
}

/// One keycode's keysyms (group 1, levels 1 and 2; 0 is `NoSymbol`), and whether the primary's
/// modifier map lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyEntry {
    pub level1: u32,
    pub level2: u32,
    pub modifier: bool,
}

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
    /// Input: the sender's monitors, whenever they change. One to [`MAX_MONITORS`].
    Layout {
        monitors: Vec<Monitor>,
    },
    /// Input, primary to secondary: the keysyms of keycodes `first..first + keys.len()`.
    Keymap {
        first: u8,
        keys: Vec<KeyEntry>,
    },
    /// Input, primary to secondary: the pointer arrives through `side` at `fraction` of its
    /// covered length (in 1/65536), with these modifier keys held (a keycode bitmap) and locks on.
    Enter {
        span: u32,
        side: Side,
        fraction: u16,
        modifiers: [u8; 32],
        locks: u8,
    },
    /// Input, primary to secondary: pointer motion in 1/256 px.
    Motion {
        span: u32,
        dx: i32,
        dy: i32,
    },
    Button {
        span: u32,
        button: u8,
        down: bool,
    },
    Key {
        span: u32,
        keycode: u8,
        down: bool,
    },
    /// Input, secondary to primary: the pointer left through `side` at `fraction`.
    Leave {
        span: u32,
        side: Side,
        fraction: u16,
    },
    /// Input, primary to secondary: the primary took the pointer back. Also sent when the primary
    /// notices silence on the session, span or not, to mark the end of what arrives late.
    Exit {
        span: u32,
    },
    /// Input, primary to secondary: the lock key locked the pointer to the secondary, or
    /// unlocked it; while locked it never leaves.
    Lock {
        span: u32,
        on: bool,
    },
    /// Input, secondary to primary: the secondary took the span `Enter` opened, and holds the
    /// primary's keymap or not. A primary that has no answer soon takes the pointer back.
    Entered {
        span: u32,
        keymap: bool,
    },
}

const HELLO: u8 = 1;
const PING: u8 = 2;
const PONG: u8 = 3;
const BYE: u8 = 4;
const COMMIT: u8 = 5;
const REVEAL: u8 = 6;
const CONFIRM: u8 = 7;
const LAYOUT: u8 = 8;
const KEYMAP: u8 = 9;
const ENTER: u8 = 10;
const MOTION: u8 = 11;
const BUTTON: u8 = 12;
const KEY: u8 = 13;
const LEAVE: u8 = 14;
const EXIT: u8 = 15;
const LOCK: u8 = 16;
const ENTERED: u8 = 17;

const MONITOR_LEN: u32 = 8;
const KEY_ENTRY_LEN: u32 = 9;

/// The longest payload of each kind, or `None` for a kind this version does not know.
fn max_payload(kind: u8) -> Option<u32> {
    // A u8 length and at most 255 bytes of name: never more than 266 in all.
    const HELLO_MAX: u32 = 2 + 8 + 1 + 255;
    match kind {
        HELLO => Some(HELLO_MAX),
        PONG | EXIT => Some(4),
        BYE | CONFIRM => Some(0),
        COMMIT | REVEAL => Some(32),
        LAYOUT => Some(1 + 16 * MONITOR_LEN),
        KEYMAP => Some(2 + 248 * KEY_ENTRY_LEN),
        ENTER => Some(4 + 1 + 2 + 32 + 1),
        MOTION => Some(12),
        PING | BUTTON | KEY => Some(6),
        LEAVE => Some(7),
        LOCK | ENTERED => Some(5),
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

fn layout_ok(monitors: &[Monitor]) -> Result<(), LimitError> {
    if monitors.is_empty() || monitors.len() > MAX_MONITORS {
        return Err(LimitError::TooManyEntries { count: monitors.len() });
    }
    monitors.iter().try_for_each(Monitor::check)
}

fn keymap_ok(first: u8, keys: &[KeyEntry]) -> Result<(), LimitError> {
    let last = usize::from(first).saturating_add(keys.len());
    if first < MIN_KEYCODE || keys.is_empty() || keys.len() > MAX_KEYMAP_KEYS || last > 256 {
        return Err(LimitError::TooManyEntries { count: keys.len() });
    }
    Ok(())
}

fn enter_ok(modifiers: &[u8; 32], locks: u8) -> Result<(), LimitError> {
    // Keycodes below 8 do not exist.
    if modifiers.first().is_some_and(|b| *b != 0) {
        return Err(LimitError::UnknownCode { code: 0 });
    }
    if locks & !(LOCK_CAPS | LOCK_NUM) != 0 {
        return Err(LimitError::UnknownCode { code: locks });
    }
    Ok(())
}

fn motion_ok(d: i32) -> Result<(), LimitError> {
    if (-MAX_MOTION..=MAX_MOTION).contains(&d) { Ok(()) } else { Err(LimitError::BadMotion) }
}

fn button_ok(button: u8) -> Result<(), LimitError> {
    if (1..=MAX_BUTTON).contains(&button) {
        Ok(())
    } else {
        Err(LimitError::UnknownCode { code: button })
    }
}

fn keycode_ok(keycode: u8) -> Result<(), LimitError> {
    if keycode >= MIN_KEYCODE { Ok(()) } else { Err(LimitError::UnknownCode { code: keycode }) }
}

/// The payload of one of input sharing's kinds into `p`, and its kind.
fn encode_input(message: &Message, p: &mut Vec<u8>) -> Result<u8, LimitError> {
    Ok(match message {
        Message::Layout { monitors } => {
            layout_ok(monitors)?;
            // At most 16, checked above.
            p.push(u8::try_from(monitors.len()).unwrap_or(u8::MAX));
            for m in monitors {
                for v in [m.x, m.y, m.width, m.height] {
                    p.extend_from_slice(&v.to_be_bytes());
                }
            }
            LAYOUT
        }
        Message::Keymap { first, keys } => {
            keymap_ok(*first, keys)?;
            p.push(*first);
            // 1 to 248, checked above; 248 travels as 248.
            p.push(u8::try_from(keys.len()).unwrap_or(u8::MAX));
            for k in keys {
                p.extend_from_slice(&k.level1.to_be_bytes());
                p.extend_from_slice(&k.level2.to_be_bytes());
                p.push(u8::from(k.modifier));
            }
            KEYMAP
        }
        Message::Enter { span, side, fraction, modifiers, locks } => {
            enter_ok(modifiers, *locks)?;
            p.extend_from_slice(&span.to_be_bytes());
            p.push(side.code());
            p.extend_from_slice(&fraction.to_be_bytes());
            p.extend_from_slice(modifiers);
            p.push(*locks);
            ENTER
        }
        Message::Motion { span, dx, dy } => {
            motion_ok(*dx)?;
            motion_ok(*dy)?;
            p.extend_from_slice(&span.to_be_bytes());
            p.extend_from_slice(&dx.to_be_bytes());
            p.extend_from_slice(&dy.to_be_bytes());
            MOTION
        }
        Message::Button { span, button, down } => {
            button_ok(*button)?;
            p.extend_from_slice(&span.to_be_bytes());
            p.push(*button);
            p.push(u8::from(*down));
            BUTTON
        }
        Message::Key { span, keycode, down } => {
            keycode_ok(*keycode)?;
            p.extend_from_slice(&span.to_be_bytes());
            p.push(*keycode);
            p.push(u8::from(*down));
            KEY
        }
        Message::Leave { span, side, fraction } => {
            p.extend_from_slice(&span.to_be_bytes());
            p.push(side.code());
            p.extend_from_slice(&fraction.to_be_bytes());
            LEAVE
        }
        Message::Exit { span } => {
            p.extend_from_slice(&span.to_be_bytes());
            EXIT
        }
        Message::Lock { span, on } => {
            p.extend_from_slice(&span.to_be_bytes());
            p.push(u8::from(*on));
            LOCK
        }
        Message::Entered { span, keymap } => {
            p.extend_from_slice(&span.to_be_bytes());
            p.push(u8::from(*keymap));
            ENTERED
        }
        // Every other kind is encoded by `encode`.
        Message::Hello { .. }
        | Message::Ping { .. }
        | Message::Pong { .. }
        | Message::Bye
        | Message::Commit { .. }
        | Message::Reveal { .. }
        | Message::Confirm => return Err(LimitError::UnknownCode { code: 0 }),
    })
}

/// Appends one framed message to `out`.
///
/// # Errors
/// A [`LimitError`] if the message breaks a limit (a long name, a ping interval out of range, an
/// empty monitor, ...); nothing is appended then.
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
        _ => encode_input(message, &mut p)?,
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

fn i32_be(r: &mut Reader<'_>) -> Result<i32, DecodeError> {
    Ok(i32::from_be_bytes(r.u32()?.to_be_bytes()))
}

fn bytes32(r: &mut Reader<'_>) -> Result<[u8; NONCE_LEN], DecodeError> {
    r.take(NONCE_LEN)?.try_into().map_err(|_| DecodeError::Truncated)
}

/// A flag byte that must be 0 or 1, so that every frame has one encoding.
fn flag(r: &mut Reader<'_>) -> Result<bool, DecodeError> {
    match r.u8()? {
        0 => Ok(false),
        1 => Ok(true),
        other => Err(DecodeError::BadFlag(other)),
    }
}

fn side(r: &mut Reader<'_>) -> Result<Side, DecodeError> {
    Ok(Side::from_code(r.u8()?)?)
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
        LAYOUT => {
            let count = usize::from(r.u8()?);
            if count == 0 || count > MAX_MONITORS {
                return Err(LimitError::TooManyEntries { count }.into());
            }
            let mut monitors = Vec::with_capacity(count);
            for _ in 0..count {
                let m = Monitor {
                    x: u16_be(&mut r)?,
                    y: u16_be(&mut r)?,
                    width: u16_be(&mut r)?,
                    height: u16_be(&mut r)?,
                };
                m.check()?;
                monitors.push(m);
            }
            Message::Layout { monitors }
        }
        KEYMAP => {
            let first = r.u8()?;
            let count = usize::from(r.u8()?);
            // Each entry's bytes are checked present before the vector grows past what arrived.
            let mut keys = Vec::with_capacity(count.min(MAX_KEYMAP_KEYS));
            for _ in 0..count {
                keys.push(KeyEntry { level1: r.u32()?, level2: r.u32()?, modifier: flag(&mut r)? });
            }
            keymap_ok(first, &keys)?;
            Message::Keymap { first, keys }
        }
        ENTER => {
            let span = r.u32()?;
            let side = side(&mut r)?;
            let fraction = u16_be(&mut r)?;
            let modifiers = bytes32(&mut r)?;
            let locks = r.u8()?;
            enter_ok(&modifiers, locks)?;
            Message::Enter { span, side, fraction, modifiers, locks }
        }
        MOTION => {
            let span = r.u32()?;
            let (dx, dy) = (i32_be(&mut r)?, i32_be(&mut r)?);
            motion_ok(dx)?;
            motion_ok(dy)?;
            Message::Motion { span, dx, dy }
        }
        BUTTON => {
            let span = r.u32()?;
            let button = r.u8()?;
            button_ok(button)?;
            Message::Button { span, button, down: flag(&mut r)? }
        }
        KEY => {
            let span = r.u32()?;
            let keycode = r.u8()?;
            keycode_ok(keycode)?;
            Message::Key { span, keycode, down: flag(&mut r)? }
        }
        LEAVE => {
            let span = r.u32()?;
            let side = side(&mut r)?;
            Message::Leave { span, side, fraction: u16_be(&mut r)? }
        }
        EXIT => Message::Exit { span: r.u32()? },
        LOCK => Message::Lock { span: r.u32()?, on: flag(&mut r)? },
        ENTERED => Message::Entered { span: r.u32()?, keymap: flag(&mut r)? },
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
