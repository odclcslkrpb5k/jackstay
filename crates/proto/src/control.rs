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

use crate::{DecodeError, FRAME_HEADER_LEN, LimitError, MAX_PAYLOAD_LEN, Reader};

/// The control protocol version this code speaks.
pub const VERSION: u16 = 2;
/// The most entries one [`Response::List`] carries; ask again with an offset for more.
pub const MAX_LIST: usize = 500;
/// The longest preview, in characters.
pub const MAX_PREVIEW_CHARS: usize = 200;
/// The longest request payload; every request is far shorter.
pub const MAX_REQUEST_PAYLOAD: u32 = 16;
/// The longest source name, in bytes.
pub const MAX_SOURCE_LEN: usize = 256;
/// The widest thumbnail, in pixels.
pub const MAX_THUMBNAIL_WIDTH: u16 = 128;
/// The tallest thumbnail, in pixels.
pub const MAX_THUMBNAIL_HEIGHT: u16 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

const WELCOME: u8 = 64;
const OK: u8 = 65;
const ERROR: u8 = 66;
const STATUS_REPLY: u8 = 67;
const LIST_REPLY: u8 = 68;
const IMPORTED: u8 = 69;
const CHANGED: u8 = 70;
const THUMBNAIL_REPLY: u8 = 71;

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

/// Appends one framed request to `out`. Requests are small, so this cannot fail.
pub fn encode_request(request: &Request, out: &mut Vec<u8>) {
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
    };
    // At most 12 bytes of payload: always within the frame limit.
    let _ = frame(kind, &p, out);
}

/// Decodes the first request in `buf`, as [`crate::decode`] does for peer messages.
///
/// # Errors
/// A [`DecodeError`] if the frame is malformed; the connection is then closed.
pub fn decode_request(buf: &[u8]) -> Result<Option<(Request, usize)>, DecodeError> {
    // No request carries more than 12 bytes: a longer one is refused from its header, before the
    // daemon buffers any of it.
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
        encode_request(&request, &mut again);
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
