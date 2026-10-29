//! The control protocol between `jackstayd` and its clients on this machine (the `jackstay` CLI,
//! later the UI), over the daemon's Unix socket (docs/daemon.md, "Control socket"). A namespace of
//! its own, versioned on its own, separate from the peer protocol; the framing is the same
//! (`[u32 BE payload length][u8 kind][payload]`, at most [`MAX_PAYLOAD_LEN`]).
//!
//! A client's first request is [`Request::Hello`]; the daemon answers [`Response::Welcome`] or
//! [`Error::UnsupportedVersion`]. Then each request gets exactly one response, in order, except
//! that after [`Request::Subscribe`] the daemon also sends [`Response::Changed`] whenever the
//! history changes.
//!
//! Version 2 added [`Request::Thumbnail`]: an image entry's picture, decoded by the daemon, so the
//! UI never decodes clipboard bytes (docs/ui.md, "Thumbnails").
//!
//! Version 3 added the link (docs/link.md, "In the daemon"): [`Request::Link`] for its state, and
//! pairing as a flow. After [`Request::Pair`] is answered `Ok`, the daemon sends that client
//! [`Response::PairCode`], then [`Response::Paired`] or [`Response::PairFailed`], whenever they
//! happen; the client answers the code with [`Request::PairAnswer`], an ordinary request. Pairing
//! ends if the client disconnects.

use crate::{DecodeError, FRAME_HEADER_LEN, LimitError, MAX_PAYLOAD_LEN, Reader};

/// The control protocol version this code speaks.
pub const VERSION: u16 = 3;
/// The oldest version the daemon still answers: each version adds to the last, so a client of an
/// older one (a UI still running across an upgrade) keeps working.
pub const OLDEST_VERSION: u16 = 2;
/// The most entries one [`Response::List`] carries; ask again with an offset for more.
pub const MAX_LIST: usize = 500;
/// The longest preview, in characters.
pub const MAX_PREVIEW_CHARS: usize = 200;
/// The longest request payload: a host or peer name ([`MAX_NAME_FIELD`]) and its length. Refused
/// from the header.
pub const MAX_REQUEST_PAYLOAD: u32 = 2 + 255;
/// The longest host given to [`Request::Pair`], or peer to [`Request::Unpair`], in bytes.
pub const MAX_NAME_FIELD: usize = 255;
/// The most peers a [`LinkStatus`] lists.
pub const MAX_PEERS: usize = 16;
/// The longest text of a [`LinkStatus`] or [`Response::PairFailed`] field, in bytes.
pub const MAX_LINK_TEXT: usize = 512;
/// The longest source name, in bytes.
pub const MAX_SOURCE_LEN: usize = 256;
/// The widest thumbnail, in pixels.
pub const MAX_THUMBNAIL_WIDTH: u16 = 128;
/// The tallest thumbnail, in pixels.
pub const MAX_THUMBNAIL_HEIGHT: u16 = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Hello {
        version: u16,
    },
    Status,
    /// Entries newest first, from `offset`, at most `limit` (and at most [`MAX_LIST`]).
    List {
        offset: u32,
        limit: u32,
    },
    Select {
        id: u64,
    },
    Delete {
        id: u64,
    },
    Pin {
        id: u64,
        pinned: bool,
    },
    /// Clears the clipboard (and PRIMARY), deleting the entry on it unless pinned.
    Clear,
    /// Deletes every unpinned entry, clears the clipboard, and removes every leftover on disk.
    ClearHistory,
    /// Imports clipman's saved history; with `remove`, deletes clipman's files afterwards.
    ImportClipman {
        remove: bool,
    },
    Subscribe,
    /// The thumbnail of an entry's image.
    Thumbnail {
        id: u64,
    },
    /// The link's state ([`Response::Link`]).
    Link,
    /// Starts pairing with the machine at `host` (`host` or `host:port`).
    Pair {
        host: String,
    },
    /// The user's answer to [`Response::PairCode`].
    PairAnswer {
        yes: bool,
    },
    /// Forgets a paired machine, named by its name or its fingerprint.
    Unpair {
        peer: String,
    },
}

/// Why a request failed. Clients branch on the code (ER-8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    UnsupportedVersion,
    /// A request before `Hello`, or a second `Hello`.
    BadSequence,
    NoSuchEntry,
    TooManyPinned,
    /// The change is made in memory but could not be saved; for a clear, the files may remain.
    StoreFailed,
    /// Another client owns the clipboard, so the entry could not be served.
    ClaimFailed,
    /// clipman's files could not be read.
    ImportFailed,
    /// A fault in the daemon; it is in the journal.
    Internal,
    /// The link is off: disabled in the configuration, or it could not start.
    LinkOff,
    /// No paired machine has that name or fingerprint.
    NoSuchPeer,
    /// More than one paired machine has that name: use the fingerprint.
    AmbiguousPeer,
    /// Not pairing, or pairing from another client.
    NotPairing,
    /// The peers file could not be saved; nothing changed.
    SaveFailed,
}

impl Error {
    /// What the error means to a person, the same in every client.
    #[must_use]
    pub fn explain(self) -> &'static str {
        match self {
            Error::NoSuchEntry => "no such entry",
            Error::TooManyPinned => "too many pinned entries (history.max_pinned)",
            Error::StoreFailed => {
                "done in memory, but the history on disk could not be updated (see the journal)"
            }
            Error::ClaimFailed => {
                "another application holds the clipboard; it was not cleared or replaced"
            }
            Error::ImportFailed => "clipman's history could not be read or removed",
            Error::UnsupportedVersion | Error::BadSequence => "jackstayd refused the request",
            Error::Internal => "jackstayd failed (see the journal)",
            Error::LinkOff => "the link is off (see `jackstay peers`)",
            Error::NoSuchPeer => "no paired machine by that name or fingerprint",
            Error::AmbiguousPeer => {
                "more than one paired machine has that name; give its fingerprint"
            }
            Error::NotPairing => "not pairing",
            Error::SaveFailed => "the peers file could not be saved; nothing changed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub entries: u32,
    pub pinned: u32,
    /// The entry on the clipboard now, if Jackstay knows it.
    pub current: Option<u64>,
    pub owns_clipboard: bool,
    /// Whether the last save failed, so the history on disk is behind.
    pub store_failing: bool,
}

/// The link, as `jackstay peers` and `jackstay status` show it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LinkStatus {
    /// Why the link is not running, if it is not.
    pub off: Option<String>,
    /// The UDP port it listens on, once bound.
    pub port: Option<u16>,
    /// Why the port could not be bound (retried).
    pub bind_error: Option<String>,
    /// This machine's fingerprint.
    pub fingerprint: String,
    pub pairing: bool,
    pub peers: Vec<PeerSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerSummary {
    pub name: String,
    pub fingerprint: String,
    pub address: String,
    pub state: PeerState,
    /// The last round trip, in microseconds.
    pub rtt_us: Option<u32>,
    /// Sessions kept since the link started.
    pub connects: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerState {
    Connected,
    Suspect,
    Dialing,
    /// It does not know our key, or showed one we do not know.
    RefusedKey,
    /// It runs another version of Jackstay.
    RefusedVersion,
}

/// Why pairing ended without recording a machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairReason {
    Busy,
    NoAddress,
    Bind,
    Timeout,
    Declined,
    PeerEnded,
    SecondAttempt,
    BadReveal,
    TooManyPeers,
    Save,
    Cancelled,
    LinkOff,
    /// What was given is not a host, `host:port` or IP address.
    BadAddress,
}

impl PairReason {
    /// What the reason means to a person.
    #[must_use]
    pub fn explain(self) -> &'static str {
        match self {
            PairReason::Busy => "pairing is already running",
            PairReason::NoAddress => "the address resolves to nothing",
            PairReason::Bind => "the pairing port could not be opened",
            PairReason::Timeout => "the other machine did not pair, or no answer came, in time",
            PairReason::Declined => "declined here",
            PairReason::PeerEnded => "the other machine ended pairing",
            PairReason::SecondAttempt => {
                "a second pairing attempt arrived; pairing stopped in case it is an attack"
            }
            PairReason::BadReveal => "the other machine broke the pairing protocol",
            PairReason::TooManyPeers => "already paired with the most machines allowed",
            PairReason::Save => "the peers file could not be saved",
            PairReason::Cancelled => "cancelled",
            PairReason::LinkOff => "the link is off",
            PairReason::BadAddress => "that is not a host name, host:port or IP address",
        }
    }
}

/// What kinds of content an entry holds, as bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Kinds(pub u8);

impl Kinds {
    pub const TEXT: u8 = 1;
    pub const IMAGE: u8 = 2;
    pub const FILES: u8 = 4;
    pub const RICH_TEXT: u8 = 8;
    pub const OTHER: u8 = 16;
    const ALL: u8 = 31;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub id: u64,
    pub copied_at_ms: u64,
    pub pinned: bool,
    pub current: bool,
    pub kinds: Kinds,
    /// Bytes held across every target.
    pub size: u64,
    /// The owner's `WM_CLASS`, at most [`MAX_SOURCE_LEN`] bytes.
    pub source: Option<String>,
    /// The start of the text, if any, at most [`MAX_PREVIEW_CHARS`] characters.
    pub preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Welcome {
        version: u16,
    },
    Ok,
    Error(Error),
    Status(Status),
    /// `total` entries exist; these are from the requested offset.
    List {
        total: u32,
        entries: Vec<Summary>,
    },
    Imported {
        added: u32,
        skipped: u32,
    },
    /// The history changed (sent only after `Subscribe`).
    Changed,
    Thumbnail {
        id: u64,
        thumbnail: Thumb,
    },
    Link(LinkStatus),
    /// Pairing reached the other machine: show the code and its fingerprint, and ask.
    PairCode {
        name: String,
        fingerprint: String,
        code: String,
    },
    Paired {
        name: String,
        fingerprint: String,
    },
    PairFailed {
        reason: PairReason,
        /// More, for some reasons (the I/O error of a save or bind).
        detail: String,
    },
}

/// What the daemon has of an entry's picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Thumb {
    /// None: no such entry, no image in it, or none of its images could be decoded.
    Absent,
    /// Still being made; ask again.
    Pending,
    Ready(Thumbnail),
}

/// An image scaled down to fit [`MAX_THUMBNAIL_WIDTH`] by [`MAX_THUMBNAIL_HEIGHT`], never up, as
/// rows of 8-bit RGBA with straight alpha. Built only through [`Thumbnail::new`], so its size and
/// its pixels always agree. Its `Debug` shows sizes, never pixels: they are clipboard contents.
#[derive(Clone, PartialEq, Eq)]
pub struct Thumbnail {
    width: u16,
    height: u16,
    image_width: u32,
    image_height: u32,
    rgba: Vec<u8>,
}

impl std::fmt::Debug for Thumbnail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Thumbnail({}x{} of {}x{})",
            self.width, self.height, self.image_width, self.image_height
        )
    }
}

/// The bytes before a thumbnail's pixels: its size and its image's.
const THUMBNAIL_HEADER_LEN: usize = 12;

impl Thumbnail {
    /// A `width` by `height` thumbnail of an `image_width` by `image_height` image.
    ///
    /// # Errors
    /// [`LimitError::BadThumbnail`] if a side is 0 or past its limit, the thumbnail is larger than
    /// its image, or `rgba` is not 4 bytes a pixel.
    pub fn new(
        width: u16,
        height: u16,
        image_width: u32,
        image_height: u32,
        rgba: Vec<u8>,
    ) -> Result<Self, LimitError> {
        let fits = (1..=MAX_THUMBNAIL_WIDTH).contains(&width)
            && (1..=MAX_THUMBNAIL_HEIGHT).contains(&height)
            && image_width >= u32::from(width)
            && image_height >= u32::from(height);
        let len =
            usize::from(width).checked_mul(usize::from(height)).and_then(|p| p.checked_mul(4));
        if !fits || len != Some(rgba.len()) {
            return Err(LimitError::BadThumbnail);
        }
        Ok(Thumbnail { width, height, image_width, image_height, rgba })
    }

    #[must_use]
    pub fn width(&self) -> u16 {
        self.width
    }

    #[must_use]
    pub fn height(&self) -> u16 {
        self.height
    }

    /// The width of the image it shows.
    #[must_use]
    pub fn image_width(&self) -> u32 {
        self.image_width
    }

    /// The height of the image it shows.
    #[must_use]
    pub fn image_height(&self) -> u32 {
        self.image_height
    }

    /// The pixels, row by row, 4 bytes each.
    #[must_use]
    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    /// Appends its encoding: width and height (u16), the image's (u32), then the pixels.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.width.to_be_bytes());
        out.extend_from_slice(&self.height.to_be_bytes());
        out.extend_from_slice(&self.image_width.to_be_bytes());
        out.extend_from_slice(&self.image_height.to_be_bytes());
        out.extend_from_slice(&self.rgba);
    }

    /// Decodes exactly one encoded thumbnail: every byte of `bytes`, no more, no fewer.
    ///
    /// # Errors
    /// A [`DecodeError`] if it is truncated, followed by other bytes, or breaks a limit.
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(bytes);
        let thumbnail = Self::read(&mut r)?;
        r.finish()?;
        Ok(thumbnail)
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let width = u16_be(r)?;
        let height = u16_be(r)?;
        let image_width = r.u32()?;
        let image_height = r.u32()?;
        // At most 128 x 64 x 4: checked before anything is taken.
        let len = usize::from(width.min(MAX_THUMBNAIL_WIDTH))
            .saturating_mul(usize::from(height.min(MAX_THUMBNAIL_HEIGHT)))
            .saturating_mul(4);
        let rgba = r.take(len)?.to_vec();
        Ok(Thumbnail::new(width, height, image_width, image_height, rgba)?)
    }

    /// The longest encoding, in bytes.
    pub const MAX_ENCODED_LEN: usize =
        THUMBNAIL_HEADER_LEN + MAX_THUMBNAIL_WIDTH as usize * MAX_THUMBNAIL_HEIGHT as usize * 4;
}

const HELLO: u8 = 1;
const STATUS: u8 = 2;
const LIST: u8 = 3;
const SELECT: u8 = 4;
const DELETE: u8 = 5;
const PIN: u8 = 6;
const CLEAR: u8 = 7;
const CLEAR_HISTORY: u8 = 8;
const IMPORT_CLIPMAN: u8 = 9;
const SUBSCRIBE: u8 = 10;
const THUMBNAIL: u8 = 11;
const LINK: u8 = 12;
const PAIR: u8 = 13;
const PAIR_ANSWER: u8 = 14;
const UNPAIR: u8 = 15;

const WELCOME: u8 = 64;
const OK: u8 = 65;
const ERROR: u8 = 66;
const STATUS_REPLY: u8 = 67;
const LIST_REPLY: u8 = 68;
const IMPORTED: u8 = 69;
const CHANGED: u8 = 70;
const THUMBNAIL_REPLY: u8 = 71;
const LINK_REPLY: u8 = 72;
const PAIR_CODE: u8 = 73;
const PAIRED: u8 = 74;
const PAIR_FAILED: u8 = 75;

const ABSENT: u8 = 0;
const PENDING: u8 = 1;
const READY: u8 = 2;

fn frame(kind: u8, payload: &[u8], out: &mut Vec<u8>) -> Result<(), LimitError> {
    let len = u32::try_from(payload.len())
        .ok()
        .filter(|&l| l <= MAX_PAYLOAD_LEN)
        .ok_or(LimitError::PayloadTooLarge { len: payload.len() })?;
    out.extend_from_slice(&len.to_be_bytes());
    out.push(kind);
    out.extend_from_slice(payload);
    Ok(())
}

/// The first frame in `buf`: its kind, its payload, and the bytes it used; `None` if `buf` does not
/// yet hold a whole frame. The length is checked before anything waits for that many bytes.
/// A whole frame: its kind, its payload, and the bytes it used.
type Framed<'a> = (u8, &'a [u8], usize);

fn unframe(buf: &[u8]) -> Result<Option<Framed<'_>>, DecodeError> {
    let mut r = Reader::new(buf);
    let Ok(len) = r.u32() else { return Ok(None) };
    if len > MAX_PAYLOAD_LEN {
        return Err(DecodeError::FrameTooLarge { len });
    }
    let Ok(kind) = r.u8() else { return Ok(None) };
    let payload_len = usize::try_from(len).map_err(|_| DecodeError::FrameTooLarge { len })?;
    let Ok(payload) = r.take(payload_len) else { return Ok(None) };
    let used =
        FRAME_HEADER_LEN.checked_add(payload_len).ok_or(DecodeError::FrameTooLarge { len })?;
    Ok(Some((kind, payload, used)))
}

fn flag(r: &mut Reader<'_>) -> Result<bool, DecodeError> {
    match r.u8()? {
        0 => Ok(false),
        1 => Ok(true),
        other => Err(DecodeError::BadFlag(other)),
    }
}

fn u16_be(r: &mut Reader<'_>) -> Result<u16, DecodeError> {
    let b = r.take(2)?;
    let pair: [u8; 2] = b.try_into().map_err(|_| DecodeError::Truncated)?;
    Ok(u16::from_be_bytes(pair))
}

/// Appends one framed request to `out`.
///
/// # Errors
/// [`LimitError::TextTooLong`] if a host or peer name is longer than [`MAX_NAME_FIELD`]; nothing
/// is appended then.
pub fn encode_request(request: &Request, out: &mut Vec<u8>) -> Result<(), LimitError> {
    let mut p = Vec::new();
    let kind = match *request {
        Request::Hello { version } => {
            p.extend_from_slice(&version.to_be_bytes());
            HELLO
        }
        Request::Status => STATUS,
        Request::List { offset, limit } => {
            p.extend_from_slice(&offset.to_be_bytes());
            p.extend_from_slice(&limit.to_be_bytes());
            LIST
        }
        Request::Select { id } => {
            p.extend_from_slice(&id.to_be_bytes());
            SELECT
        }
        Request::Delete { id } => {
            p.extend_from_slice(&id.to_be_bytes());
            DELETE
        }
        Request::Pin { id, pinned } => {
            p.extend_from_slice(&id.to_be_bytes());
            p.push(u8::from(pinned));
            PIN
        }
        Request::Clear => CLEAR,
        Request::ClearHistory => CLEAR_HISTORY,
        Request::ImportClipman { remove } => {
            p.push(u8::from(remove));
            IMPORT_CLIPMAN
        }
        Request::Subscribe => SUBSCRIBE,
        Request::Thumbnail { id } => {
            p.extend_from_slice(&id.to_be_bytes());
            THUMBNAIL
        }
        Request::Link => LINK,
        Request::Pair { ref host } => {
            put_text(&mut p, host, MAX_NAME_FIELD)?;
            PAIR
        }
        Request::PairAnswer { yes } => {
            p.push(u8::from(yes));
            PAIR_ANSWER
        }
        Request::Unpair { ref peer } => {
            put_text(&mut p, peer, MAX_NAME_FIELD)?;
            UNPAIR
        }
    };
    frame(kind, &p, out)
}

/// Decodes the first request in `buf`, as [`crate::decode`] does for peer messages.
///
/// # Errors
/// A [`DecodeError`] if the frame is malformed; the connection is then closed.
pub fn decode_request(buf: &[u8]) -> Result<Option<(Request, usize)>, DecodeError> {
    // No request carries more than a name and its length: a longer one is refused from its header,
    // before the daemon buffers any of it.
    let mut header = Reader::new(buf);
    if let Ok(len) = header.u32()
        && len > MAX_REQUEST_PAYLOAD
    {
        return Err(DecodeError::FrameTooLarge { len });
    }
    let Some((kind, payload, used)) = unframe(buf)? else { return Ok(None) };
    let mut r = Reader::new(payload);
    let request = match kind {
        HELLO => Request::Hello { version: u16_be(&mut r)? },
        STATUS => Request::Status,
        LIST => Request::List { offset: r.u32()?, limit: r.u32()? },
        SELECT => Request::Select { id: r.u64()? },
        DELETE => Request::Delete { id: r.u64()? },
        PIN => Request::Pin { id: r.u64()?, pinned: flag(&mut r)? },
        CLEAR => Request::Clear,
        CLEAR_HISTORY => Request::ClearHistory,
        IMPORT_CLIPMAN => Request::ImportClipman { remove: flag(&mut r)? },
        SUBSCRIBE => Request::Subscribe,
        THUMBNAIL => Request::Thumbnail { id: r.u64()? },
        LINK => Request::Link,
        PAIR => Request::Pair { host: text(&mut r, MAX_NAME_FIELD)? },
        PAIR_ANSWER => Request::PairAnswer { yes: flag(&mut r)? },
        UNPAIR => Request::Unpair { peer: text(&mut r, MAX_NAME_FIELD)? },
        other => return Err(DecodeError::UnknownKind(other)),
    };
    r.finish()?;
    Ok(Some((request, used)))
}

fn error_code(e: Error) -> u8 {
    match e {
        Error::UnsupportedVersion => 1,
        Error::BadSequence => 2,
        Error::NoSuchEntry => 3,
        Error::TooManyPinned => 4,
        Error::StoreFailed => 5,
        Error::ClaimFailed => 6,
        Error::ImportFailed => 7,
        Error::Internal => 8,
        Error::LinkOff => 9,
        Error::NoSuchPeer => 10,
        Error::AmbiguousPeer => 11,
        Error::NotPairing => 12,
        Error::SaveFailed => 13,
    }
}

fn error_of(code: u8) -> Result<Error, DecodeError> {
    Ok(match code {
        1 => Error::UnsupportedVersion,
        2 => Error::BadSequence,
        3 => Error::NoSuchEntry,
        4 => Error::TooManyPinned,
        5 => Error::StoreFailed,
        6 => Error::ClaimFailed,
        7 => Error::ImportFailed,
        8 => Error::Internal,
        9 => Error::LinkOff,
        10 => Error::NoSuchPeer,
        11 => Error::AmbiguousPeer,
        12 => Error::NotPairing,
        13 => Error::SaveFailed,
        other => return Err(DecodeError::Limit(LimitError::UnknownCode { code: other })),
    })
}

fn put_text(p: &mut Vec<u8>, text: &str, max: usize) -> Result<(), LimitError> {
    let too_long = LimitError::TextTooLong { len: text.len() };
    if text.len() > max {
        return Err(too_long);
    }
    let len = u16::try_from(text.len()).map_err(|_| too_long)?;
    p.extend_from_slice(&len.to_be_bytes());
    p.extend_from_slice(text.as_bytes());
    Ok(())
}

fn text(r: &mut Reader<'_>, max: usize) -> Result<String, DecodeError> {
    let len = usize::from(u16_be(r)?);
    if len > max {
        return Err(LimitError::TextTooLong { len }.into());
    }
    let bytes = r.take(len)?;
    String::from_utf8(bytes.to_vec()).map_err(|_| DecodeError::Limit(LimitError::TextNotUtf8))
}

fn put_option_text(p: &mut Vec<u8>, text: Option<&String>, max: usize) -> Result<(), LimitError> {
    match text {
        None => p.push(0),
        Some(t) => {
            p.push(1);
            put_text(p, t, max)?;
        }
    }
    Ok(())
}

fn option_text(r: &mut Reader<'_>, max: usize) -> Result<Option<String>, DecodeError> {
    Ok(if flag(r)? { Some(text(r, max)?) } else { None })
}

fn peer_state_code(s: PeerState) -> u8 {
    match s {
        PeerState::Connected => 0,
        PeerState::Suspect => 1,
        PeerState::Dialing => 2,
        PeerState::RefusedKey => 3,
        PeerState::RefusedVersion => 4,
    }
}

fn peer_state_of(code: u8) -> Result<PeerState, DecodeError> {
    Ok(match code {
        0 => PeerState::Connected,
        1 => PeerState::Suspect,
        2 => PeerState::Dialing,
        3 => PeerState::RefusedKey,
        4 => PeerState::RefusedVersion,
        code => return Err(LimitError::UnknownCode { code }.into()),
    })
}

const PAIR_REASONS: [PairReason; 13] = [
    PairReason::Busy,
    PairReason::NoAddress,
    PairReason::Bind,
    PairReason::Timeout,
    PairReason::Declined,
    PairReason::PeerEnded,
    PairReason::SecondAttempt,
    PairReason::BadReveal,
    PairReason::TooManyPeers,
    PairReason::Save,
    PairReason::Cancelled,
    PairReason::LinkOff,
    PairReason::BadAddress,
];

fn pair_reason_code(reason: PairReason) -> u8 {
    let index = PAIR_REASONS.iter().position(|r| *r == reason).unwrap_or_default();
    u8::try_from(index).unwrap_or_default()
}

fn pair_reason_of(code: u8) -> Result<PairReason, DecodeError> {
    PAIR_REASONS
        .get(usize::from(code))
        .copied()
        .ok_or(DecodeError::Limit(LimitError::UnknownCode { code }))
}

fn put_link(p: &mut Vec<u8>, l: &LinkStatus) -> Result<(), LimitError> {
    if l.peers.len() > MAX_PEERS {
        return Err(LimitError::TooManyEntries { count: l.peers.len() });
    }
    put_option_text(p, l.off.as_ref(), MAX_LINK_TEXT)?;
    match l.port {
        None => p.push(0),
        Some(port) => {
            p.push(1);
            p.extend_from_slice(&port.to_be_bytes());
        }
    }
    put_option_text(p, l.bind_error.as_ref(), MAX_LINK_TEXT)?;
    put_text(p, &l.fingerprint, MAX_LINK_TEXT)?;
    p.push(u8::from(l.pairing));
    p.push(
        u8::try_from(l.peers.len())
            .map_err(|_| LimitError::TooManyEntries { count: l.peers.len() })?,
    );
    for peer in &l.peers {
        put_text(p, &peer.name, MAX_LINK_TEXT)?;
        put_text(p, &peer.fingerprint, MAX_LINK_TEXT)?;
        put_text(p, &peer.address, MAX_LINK_TEXT)?;
        p.push(peer_state_code(peer.state));
        match peer.rtt_us {
            None => p.push(0),
            Some(us) => {
                p.push(1);
                p.extend_from_slice(&us.to_be_bytes());
            }
        }
        p.extend_from_slice(&peer.connects.to_be_bytes());
    }
    Ok(())
}

fn link(r: &mut Reader<'_>) -> Result<LinkStatus, DecodeError> {
    let off = option_text(r, MAX_LINK_TEXT)?;
    let port = if flag(r)? { Some(u16_be(r)?) } else { None };
    let bind_error = option_text(r, MAX_LINK_TEXT)?;
    let fingerprint = text(r, MAX_LINK_TEXT)?;
    let pairing = flag(r)?;
    let count = usize::from(r.u8()?);
    if count > MAX_PEERS {
        return Err(LimitError::TooManyEntries { count }.into());
    }
    let mut peers = Vec::with_capacity(count);
    for _ in 0..count {
        let name = text(r, MAX_LINK_TEXT)?;
        let fingerprint = text(r, MAX_LINK_TEXT)?;
        let address = text(r, MAX_LINK_TEXT)?;
        let state = peer_state_of(r.u8()?)?;
        let rtt_us = if flag(r)? { Some(r.u32()?) } else { None };
        let connects = r.u32()?;
        peers.push(PeerSummary { name, fingerprint, address, state, rtt_us, connects });
    }
    Ok(LinkStatus { off, port, bind_error, fingerprint, pairing, peers })
}

/// The longest preview in bytes: four per character.
const MAX_PREVIEW_BYTES: usize = MAX_PREVIEW_CHARS * 4;

/// Appends one framed response to `out`.
///
/// # Errors
/// A [`LimitError`] if the response breaks a limit (too many entries, a long source or preview);
/// nothing is appended then.
pub fn encode_response(response: &Response, out: &mut Vec<u8>) -> Result<(), LimitError> {
    let mut p = Vec::new();
    let kind = match response {
        Response::Welcome { version } => {
            p.extend_from_slice(&version.to_be_bytes());
            WELCOME
        }
        Response::Ok => OK,
        Response::Error(e) => {
            p.push(error_code(*e));
            ERROR
        }
        Response::Status(s) => {
            p.extend_from_slice(&s.entries.to_be_bytes());
            p.extend_from_slice(&s.pinned.to_be_bytes());
            match s.current {
                None => p.push(0),
                Some(id) => {
                    p.push(1);
                    p.extend_from_slice(&id.to_be_bytes());
                }
            }
            p.push(u8::from(s.owns_clipboard));
            p.push(u8::from(s.store_failing));
            STATUS_REPLY
        }
        Response::List { total, entries } => {
            if entries.len() > MAX_LIST {
                return Err(LimitError::TooManyEntries { count: entries.len() });
            }
            p.extend_from_slice(&total.to_be_bytes());
            let count = u16::try_from(entries.len())
                .map_err(|_| LimitError::TooManyEntries { count: entries.len() })?;
            p.extend_from_slice(&count.to_be_bytes());
            for e in entries {
                if e.kinds.0 & !Kinds::ALL != 0 {
                    return Err(LimitError::UnknownCode { code: e.kinds.0 });
                }
                if e.preview.chars().count() > MAX_PREVIEW_CHARS {
                    return Err(LimitError::TextTooLong { len: e.preview.len() });
                }
                p.extend_from_slice(&e.id.to_be_bytes());
                p.extend_from_slice(&e.copied_at_ms.to_be_bytes());
                p.push(u8::from(e.pinned));
                p.push(u8::from(e.current));
                p.push(e.kinds.0);
                p.extend_from_slice(&e.size.to_be_bytes());
                match &e.source {
                    None => p.push(0),
                    Some(s) => {
                        p.push(1);
                        put_text(&mut p, s, MAX_SOURCE_LEN)?;
                    }
                }
                put_text(&mut p, &e.preview, MAX_PREVIEW_BYTES)?;
            }
            LIST_REPLY
        }
        Response::Imported { added, skipped } => {
            p.extend_from_slice(&added.to_be_bytes());
            p.extend_from_slice(&skipped.to_be_bytes());
            IMPORTED
        }
        Response::Changed => CHANGED,
        Response::Thumbnail { id, thumbnail } => {
            p.extend_from_slice(&id.to_be_bytes());
            match thumbnail {
                Thumb::Absent => p.push(ABSENT),
                Thumb::Pending => p.push(PENDING),
                Thumb::Ready(t) => {
                    p.push(READY);
                    t.encode(&mut p);
                }
            }
            THUMBNAIL_REPLY
        }
        Response::Link(l) => {
            put_link(&mut p, l)?;
            LINK_REPLY
        }
        Response::PairCode { name, fingerprint, code } => {
            put_text(&mut p, name, MAX_LINK_TEXT)?;
            put_text(&mut p, fingerprint, MAX_LINK_TEXT)?;
            put_text(&mut p, code, MAX_LINK_TEXT)?;
            PAIR_CODE
        }
        Response::Paired { name, fingerprint } => {
            put_text(&mut p, name, MAX_LINK_TEXT)?;
            put_text(&mut p, fingerprint, MAX_LINK_TEXT)?;
            PAIRED
        }
        Response::PairFailed { reason, detail } => {
            p.push(pair_reason_code(*reason));
            put_text(&mut p, detail, MAX_LINK_TEXT)?;
            PAIR_FAILED
        }
    };
    frame(kind, &p, out)
}

/// Decodes the first response in `buf`.
///
/// # Errors
/// A [`DecodeError`] if the frame is malformed.
pub fn decode_response(buf: &[u8]) -> Result<Option<(Response, usize)>, DecodeError> {
    let Some((kind, payload, used)) = unframe(buf)? else { return Ok(None) };
    let mut r = Reader::new(payload);
    let response = match kind {
        WELCOME => Response::Welcome { version: u16_be(&mut r)? },
        OK => Response::Ok,
        ERROR => Response::Error(error_of(r.u8()?)?),
        STATUS_REPLY => {
            let entries = r.u32()?;
            let pinned = r.u32()?;
            let current = if flag(&mut r)? { Some(r.u64()?) } else { None };
            Response::Status(Status {
                entries,
                pinned,
                current,
                owns_clipboard: flag(&mut r)?,
                store_failing: flag(&mut r)?,
            })
        }
        LIST_REPLY => {
            let total = r.u32()?;
            let count = usize::from(u16_be(&mut r)?);
            if count > MAX_LIST {
                return Err(LimitError::TooManyEntries { count }.into());
            }
            let mut entries = Vec::with_capacity(count);
            for _ in 0..count {
                let id = r.u64()?;
                let copied_at_ms = r.u64()?;
                let pinned = flag(&mut r)?;
                let current = flag(&mut r)?;
                let kinds = r.u8()?;
                if kinds & !Kinds::ALL != 0 {
                    return Err(LimitError::UnknownCode { code: kinds }.into());
                }
                let size = r.u64()?;
                let source = if flag(&mut r)? { Some(text(&mut r, MAX_SOURCE_LEN)?) } else { None };
                let preview = text(&mut r, MAX_PREVIEW_BYTES)?;
                if preview.chars().count() > MAX_PREVIEW_CHARS {
                    return Err(LimitError::TextTooLong { len: preview.len() }.into());
                }
                entries.push(Summary {
                    id,
                    copied_at_ms,
                    pinned,
                    current,
                    kinds: Kinds(kinds),
                    size,
                    source,
                    preview,
                });
            }
            Response::List { total, entries }
        }
        IMPORTED => Response::Imported { added: r.u32()?, skipped: r.u32()? },
        CHANGED => Response::Changed,
        THUMBNAIL_REPLY => {
            let id = r.u64()?;
            let thumbnail = match r.u8()? {
                ABSENT => Thumb::Absent,
                PENDING => Thumb::Pending,
                READY => Thumb::Ready(Thumbnail::read(&mut r)?),
                code => return Err(LimitError::UnknownCode { code }.into()),
            };
            Response::Thumbnail { id, thumbnail }
        }
        LINK_REPLY => Response::Link(link(&mut r)?),
        PAIR_CODE => Response::PairCode {
            name: text(&mut r, MAX_LINK_TEXT)?,
            fingerprint: text(&mut r, MAX_LINK_TEXT)?,
            code: text(&mut r, MAX_LINK_TEXT)?,
        },
        PAIRED => Response::Paired {
            name: text(&mut r, MAX_LINK_TEXT)?,
            fingerprint: text(&mut r, MAX_LINK_TEXT)?,
        },
        PAIR_FAILED => Response::PairFailed {
            reason: pair_reason_of(r.u8()?)?,
            detail: text(&mut r, MAX_LINK_TEXT)?,
        },
        other => return Err(DecodeError::UnknownKind(other)),
    };
    r.finish()?;
    Ok(Some((response, used)))
}

/// Whether `c` changes how the text around it displays rather than showing itself: a control
/// character (newlines, tabs, terminal escapes), a format character (bidirectional overrides and
/// isolates, zero-width characters, the byte-order mark), or a line or paragraph separator.
#[must_use]
pub fn invisible(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{ad}'
                | '\u{61c}'
                | '\u{180e}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}'
                | '\u{feff}'
                | '\u{fff9}'..='\u{fffb}'
                | '\u{e0000}'..='\u{e007f}'
        )
}

/// `text` with every [`invisible`] character shown as a space, so it is one line and reads as it
/// is, wherever it is printed.
#[must_use]
pub fn printable(text: &str) -> String {
    text.chars().map(|c| if invisible(c) { ' ' } else { c }).collect()
}

/// The start of `text` for a preview: at most [`MAX_PREVIEW_CHARS`] characters, [`printable`].
#[must_use]
pub fn preview(text: &str) -> String {
    text.chars().take(MAX_PREVIEW_CHARS).map(|c| if invisible(c) { ' ' } else { c }).collect()
}

/// What must hold of any bytes, checked by the fuzz target and by the replay of its corpus (TS-16):
/// decoding as a request and as a response never panics; a decoded frame needs every byte it used,
/// and re-encodes to exactly them. Returns what failed.
///
/// # Errors
/// The first property that does not hold.
#[doc(hidden)]
pub fn check_decoders(data: &[u8]) -> Result<(), String> {
    if let Ok(Some((request, used))) = decode_request(data) {
        let mut again = Vec::new();
        encode_request(&request, &mut again).map_err(|e| format!("re-encoding refused: {e}"))?;
        if data.get(..used) != Some(again.as_slice()) {
            return Err("a request re-encodes differently".to_owned());
        }
        let shorter = used.checked_sub(1).and_then(|n| data.get(..n)).unwrap_or_default();
        if decode_request(shorter) != Ok(None) {
            return Err("a request decoded without all its bytes".to_owned());
        }
    }
    if let Ok(Some((response, used))) = decode_response(data) {
        let mut again = Vec::new();
        encode_response(&response, &mut again).map_err(|e| format!("re-encoding refused: {e}"))?;
        if data.get(..used) != Some(again.as_slice()) {
            return Err("a response re-encodes differently".to_owned());
        }
        let shorter = used.checked_sub(1).and_then(|n| data.get(..n)).unwrap_or_default();
        if decode_response(shorter) != Ok(None) {
            return Err("a response decoded without all its bytes".to_owned());
        }
    }
    Ok(())
}
