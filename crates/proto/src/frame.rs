use crate::{Announce, DecodeError, Reader};

/// Frame header: payload length (u32, big-endian), then message kind (u8).
pub const FRAME_HEADER_LEN: usize = 5;
/// The largest payload a control frame may carry. Clipboard data beyond the inline limit travels
/// on its own stream, not in control frames.
pub const MAX_PAYLOAD_LEN: u32 = 1024 * 1024;

const KIND_ANNOUNCE: u8 = 1;

/// Every message a control stream carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Announce(Announce),
}

/// Appends one framed message to `out`.
pub fn encode(message: &Message, out: &mut Vec<u8>) {
    match message {
        Message::Announce(a) => {
            out.extend_from_slice(&a.payload_len().to_be_bytes());
            out.push(KIND_ANNOUNCE);
            a.encode_payload(out);
        }
    }
}

/// Decodes the first frame in `buf`.
///
/// Returns `Ok(None)` when `buf` does not yet hold a whole frame, and otherwise the message and
/// the number of bytes it used. The announced length is checked against [`MAX_PAYLOAD_LEN`]
/// before anything waits for, or allocates, that many bytes.
///
/// # Errors
/// A [`DecodeError`] if the frame is malformed; the stream is then unusable.
pub fn decode(buf: &[u8]) -> Result<Option<(Message, usize)>, DecodeError> {
    let mut r = Reader::new(buf);
    let Ok(len) = r.u32() else { return Ok(None) };
    if len > MAX_PAYLOAD_LEN {
        return Err(DecodeError::FrameTooLarge { len });
    }
    let Ok(kind) = r.u8() else { return Ok(None) };
    let payload_len = usize::try_from(len).map_err(|_| DecodeError::FrameTooLarge { len })?;
    let Ok(payload) = r.take(payload_len) else { return Ok(None) };
    let message = match kind {
        KIND_ANNOUNCE => Message::Announce(Announce::decode_payload(payload)?),
        other => return Err(DecodeError::UnknownKind(other)),
    };
    let used =
        FRAME_HEADER_LEN.checked_add(payload_len).ok_or(DecodeError::FrameTooLarge { len })?;
    Ok(Some((message, used)))
}
