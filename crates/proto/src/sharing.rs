//! Clipboard sharing's messages on the link (docs/sharing.md, "Messages"): each travels on a
//! stream of its own, never stream 0. Their encoding and decoding, called from [`crate::link`].

use crate::link::{Message, flag, u16_be};
use crate::{DecodeError, LimitError, Reader};

/// The longest target or type name, in bytes, as `jackstay-clip` keeps them.
pub const MAX_NAME_LEN: usize = 4096;
/// The most targets in an [`Announce`].
pub const MAX_TARGETS: usize = 255;
/// The largest target that travels inside an [`Announce`].
pub const MAX_INLINE_LEN: usize = 64 * 1024;
/// The most bytes inline in one [`Announce`], all its targets together.
pub const MAX_INLINE_TOTAL: usize = 512 * 1024;
/// The largest [`Announce`] payload: names and inline bytes beyond it are the sender's to leave
/// out.
pub const MAX_ANNOUNCE_LEN: u32 = 1024 * 1024;
/// The most digests in one [`Message::Forget`].
pub const MAX_FORGETS: usize = 1000;

/// A SHA-256: of a target's bytes, or an entry's digest (docs/sharing.md, "Shape").
pub type Sha256 = [u8; 32];

/// A target or type name: Latin-1, 1 to [`MAX_NAME_LEN`] bytes, no NUL, as X atom names are.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Name(Vec<u8>);

impl Name {
    /// # Errors
    /// [`LimitError::BadName`] if `latin1` is empty, too long, or holds NUL.
    pub fn new(latin1: &[u8]) -> Result<Name, LimitError> {
        if latin1.is_empty() || latin1.len() > MAX_NAME_LEN || latin1.contains(&0) {
            return Err(LimitError::BadName);
        }
        Ok(Name(latin1.to_vec()))
    }

    #[must_use]
    pub fn latin1(&self) -> &[u8] {
        &self.0
    }

    fn put(&self, p: &mut Vec<u8>) {
        // At most MAX_NAME_LEN, checked in new.
        p.extend_from_slice(&u16::try_from(self.0.len()).unwrap_or(u16::MAX).to_be_bytes());
        p.extend_from_slice(&self.0);
    }

    fn read(r: &mut Reader<'_>) -> Result<Name, DecodeError> {
        let len = usize::from(u16_be(r)?);
        Ok(Name::new(r.take(len)?)?)
    }
}

/// One target of an announced entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub name: Name,
    /// The property type it is served as, such as `UTF8_STRING` or `ATOM`.
    pub type_name: Name,
    /// 8, 16 or 32.
    pub format: u8,
    pub size: u64,
    pub sha256: Sha256,
    /// The bytes, if they travel with the announcement; otherwise fetched.
    pub inline: Option<Vec<u8>>,
}

/// An entry that is the clipboard now, or was (docs/sharing.md, "Capture and announce").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announce {
    /// When the change happened, on the hybrid clock (docs/sharing.md, "Order").
    pub tick: u64,
    /// The entry's digest. Checked against its targets by the receiver, not here: this crate
    /// does not hash.
    pub digest: Sha256,
    /// The digest of the same capture announced early, which this one completes.
    pub replaces: Option<Sha256>,
    /// Wall-clock milliseconds when it was copied, on the origin.
    pub copied_at_ms: u64,
    /// 1 to [`MAX_TARGETS`], no name twice.
    pub targets: Vec<Target>,
}

impl Announce {
    /// # Errors
    /// The limit it breaks: no targets or too many, a name twice, a format other than 8, 16 or
    /// 32, inline bytes past their limits or not of the target's size.
    pub fn check(&self) -> Result<(), LimitError> {
        let count = self.targets.len();
        if count == 0 {
            return Err(LimitError::NoTargets);
        }
        if count > MAX_TARGETS {
            return Err(LimitError::TooManyTargets { count });
        }
        let mut inline_total = 0usize;
        for (i, t) in self.targets.iter().enumerate() {
            if !matches!(t.format, 8 | 16 | 32) {
                return Err(LimitError::UnknownCode { code: t.format });
            }
            if self.targets.iter().take(i).any(|o| o.name == t.name) {
                return Err(LimitError::DuplicateTarget);
            }
            if let Some(bytes) = &t.inline {
                if bytes.len() > MAX_INLINE_LEN {
                    return Err(LimitError::InlineTooLarge { len: bytes.len() });
                }
                if u64::try_from(bytes.len()).ok() != Some(t.size) {
                    return Err(LimitError::InlineSizeMismatch {
                        size: t.size,
                        inline_len: bytes.len(),
                    });
                }
                inline_total = inline_total.saturating_add(bytes.len());
                if inline_total > MAX_INLINE_TOTAL {
                    return Err(LimitError::InlineTooLarge { len: inline_total });
                }
            }
        }
        Ok(())
    }
}

pub(crate) const ANNOUNCE: u8 = 18;
pub(crate) const EMPTIED: u8 = 19;
pub(crate) const FORGET: u8 = 20;
pub(crate) const FETCH: u8 = 21;
pub(crate) const ANSWER: u8 = 22;

/// The longest payload of each of clipboard sharing's kinds.
pub(crate) fn max_payload(kind: u8) -> Option<u32> {
    // A u16 count and the digests.
    const FORGET_MAX: u32 = 8 + 2 + 32 * 1000;
    // A digest and a name.
    const FETCH_MAX: u32 = 32 + 2 + 4096;
    match kind {
        ANNOUNCE => Some(MAX_ANNOUNCE_LEN),
        EMPTIED => Some(8),
        FORGET => Some(FORGET_MAX),
        FETCH => Some(FETCH_MAX),
        ANSWER => Some(1),
        _ => None,
    }
}

fn forget_ok(digests: &[Sha256]) -> Result<(), LimitError> {
    if digests.is_empty() || digests.len() > MAX_FORGETS {
        return Err(LimitError::TooManyEntries { count: digests.len() });
    }
    Ok(())
}

/// The payload of one of clipboard sharing's kinds into `p`, and its kind; `None` for any other
/// message.
pub(crate) fn encode(message: &Message, p: &mut Vec<u8>) -> Result<Option<u8>, LimitError> {
    Ok(Some(match message {
        Message::Announce(a) => {
            a.check()?;
            p.extend_from_slice(&a.tick.to_be_bytes());
            p.extend_from_slice(&a.digest);
            match &a.replaces {
                Some(d) => {
                    p.push(1);
                    p.extend_from_slice(d);
                }
                None => p.push(0),
            }
            p.extend_from_slice(&a.copied_at_ms.to_be_bytes());
            // 1 to 255, checked above.
            p.push(u8::try_from(a.targets.len()).unwrap_or(u8::MAX));
            for t in &a.targets {
                t.name.put(p);
                t.type_name.put(p);
                p.push(t.format);
                p.extend_from_slice(&t.size.to_be_bytes());
                p.extend_from_slice(&t.sha256);
                match &t.inline {
                    Some(bytes) => {
                        p.push(1);
                        p.extend_from_slice(bytes);
                    }
                    None => p.push(0),
                }
            }
            if p.len() > usize::try_from(MAX_ANNOUNCE_LEN).unwrap_or(usize::MAX) {
                return Err(LimitError::PayloadTooLarge { len: p.len() });
            }
            ANNOUNCE
        }
        Message::Emptied { tick } => {
            p.extend_from_slice(&tick.to_be_bytes());
            EMPTIED
        }
        Message::Forget { tick, digests } => {
            forget_ok(digests)?;
            p.extend_from_slice(&tick.to_be_bytes());
            // At most 1000, checked above.
            p.extend_from_slice(&u16::try_from(digests.len()).unwrap_or(u16::MAX).to_be_bytes());
            for d in digests {
                p.extend_from_slice(d);
            }
            FORGET
        }
        Message::Fetch { digest, name } => {
            p.extend_from_slice(digest);
            name.put(p);
            FETCH
        }
        Message::Answer { held } => {
            p.push(u8::from(*held));
            ANSWER
        }
        _ => return Ok(None),
    }))
}

fn sha(r: &mut Reader<'_>) -> Result<Sha256, DecodeError> {
    r.take(32)?.try_into().map_err(|_| DecodeError::Truncated)
}

/// One of clipboard sharing's kinds from its payload; `None` for any other kind.
pub(crate) fn decode(kind: u8, r: &mut Reader<'_>) -> Result<Option<Message>, DecodeError> {
    Ok(Some(match kind {
        ANNOUNCE => {
            let tick = r.u64()?;
            let digest = sha(r)?;
            let replaces = if flag(r)? { Some(sha(r)?) } else { None };
            let copied_at_ms = r.u64()?;
            let count = usize::from(r.u8()?);
            if count == 0 {
                return Err(LimitError::NoTargets.into());
            }
            let mut targets = Vec::with_capacity(count);
            for _ in 0..count {
                let name = Name::read(r)?;
                let type_name = Name::read(r)?;
                let format = r.u8()?;
                let size = r.u64()?;
                let sha256 = sha(r)?;
                let inline = if flag(r)? {
                    // Checked before the bytes are taken, so a size past the limit never reads.
                    let len = usize::try_from(size).unwrap_or(usize::MAX);
                    if len > MAX_INLINE_LEN {
                        return Err(LimitError::InlineTooLarge { len }.into());
                    }
                    Some(r.take(len)?.to_vec())
                } else {
                    None
                };
                targets.push(Target { name, type_name, format, size, sha256, inline });
            }
            let a = Announce { tick, digest, replaces, copied_at_ms, targets };
            a.check()?;
            Message::Announce(Box::new(a))
        }
        EMPTIED => Message::Emptied { tick: r.u64()? },
        FORGET => {
            let tick = r.u64()?;
            let count = usize::from(u16_be(r)?);
            if count == 0 || count > MAX_FORGETS {
                return Err(LimitError::TooManyEntries { count }.into());
            }
            let mut digests = Vec::with_capacity(count);
            for _ in 0..count {
                digests.push(sha(r)?);
            }
            Message::Forget { tick, digests }
        }
        FETCH => Message::Fetch { digest: sha(r)?, name: Name::read(r)? },
        ANSWER => Message::Answer { held: flag(r)? },
        _ => return Ok(None),
    }))
}
