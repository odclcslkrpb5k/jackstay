use crate::{DecodeError, LimitError, MAX_PAYLOAD_LEN, Reader};

/// The most targets one entry may offer. Real owners offer a few dozen at most.
pub const MAX_TARGETS: usize = 64;
/// The longest target name, in bytes. X atom names are short ASCII.
pub const MAX_TARGET_NAME_LEN: usize = 255;
/// The largest target that may travel inside the announcement. Whether a target that fits does
/// is the sender's choice; larger ones are always fetched on demand.
pub const MAX_INLINE_LEN: usize = 64 * 1024;

/// Identifies one clipboard entry across machines. Only [`Announce`] decoding and the history
/// store mint these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EntryId(u64);

impl EntryId {
    #[must_use]
    pub fn new(raw: u64) -> Self {
        EntryId(raw)
    }

    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

/// A selection target name, such as `UTF8_STRING` or `image/png`: non-empty printable ASCII, at
/// most [`MAX_TARGET_NAME_LEN`] bytes. Its length is kept as a `u8`, which the limit guarantees.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TargetName {
    name: String,
    len: u8,
}

impl TargetName {
    /// # Errors
    /// A [`LimitError`] if the name is empty, too long, or not printable ASCII.
    pub fn parse(bytes: &[u8]) -> Result<Self, LimitError> {
        if bytes.is_empty() {
            return Err(LimitError::TargetNameEmpty);
        }
        let too_long = LimitError::TargetNameTooLong { len: bytes.len() };
        if bytes.len() > MAX_TARGET_NAME_LEN {
            return Err(too_long);
        }
        let len = u8::try_from(bytes.len()).map_err(|_| too_long)?;
        if !bytes.iter().all(u8::is_ascii_graphic) {
            return Err(LimitError::TargetNameNotPrintableAscii);
        }
        let name =
            std::str::from_utf8(bytes).map_err(|_| LimitError::TargetNameNotPrintableAscii)?;
        Ok(TargetName { name: name.to_owned(), len })
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.name
    }
}

/// One target an entry offers: its name, its size, and its bytes if small enough to travel inline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    name: TargetName,
    size: u64,
    inline: Option<Inline>,
}

/// Inline bytes, at most [`MAX_INLINE_LEN`]; the length is kept as the `u32` the wire carries.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Inline {
    data: Vec<u8>,
    len: u32,
}

impl Target {
    /// A target whose bytes travel with the announcement.
    ///
    /// # Errors
    /// [`LimitError::InlineTooLarge`] if `data` is longer than [`MAX_INLINE_LEN`].
    pub fn inline(name: TargetName, data: Vec<u8>) -> Result<Self, LimitError> {
        let too_large = LimitError::InlineTooLarge { len: data.len() };
        if data.len() > MAX_INLINE_LEN {
            return Err(too_large);
        }
        let len = u32::try_from(data.len()).map_err(|_| too_large.clone())?;
        Ok(Target { name, size: u64::from(len), inline: Some(Inline { data, len }) })
    }

    /// A target whose bytes are fetched on demand.
    #[must_use]
    pub fn deferred(name: TargetName, size: u64) -> Self {
        Target { name, size, inline: None }
    }

    #[must_use]
    pub fn name(&self) -> &TargetName {
        &self.name
    }

    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    #[must_use]
    pub fn inline_data(&self) -> Option<&[u8]> {
        self.inline.as_ref().map(|i| i.data.as_slice())
    }

    // Name length u8, name, size u64, inline flag u8, and when inline: length u32 and the bytes.
    fn encoded_len(&self) -> usize {
        let fixed = usize::from(self.name.len).saturating_add(1 + 8 + 1);
        match &self.inline {
            None => fixed,
            Some(i) => fixed.saturating_add(4).saturating_add(i.data.len()),
        }
    }
}

/// A machine telling its peers that it captured a new clipboard entry. A valid announcement always
/// fits in one control frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announce {
    id: EntryId,
    targets: Vec<Target>,
    count: u8,
    payload_len: u32,
}

impl Announce {
    /// # Errors
    /// [`LimitError::NoTargets`], [`LimitError::TooManyTargets`],
    /// [`LimitError::DuplicateTarget`] if two targets share a name (a receiver could not tell which
    /// one to serve), or [`LimitError::PayloadTooLarge`] if the encoded announcement would not fit
    /// in a frame.
    pub fn new(id: EntryId, targets: Vec<Target>) -> Result<Self, LimitError> {
        if targets.is_empty() {
            return Err(LimitError::NoTargets);
        }
        let mut names = std::collections::HashSet::with_capacity(targets.len());
        if let Some(dup) = targets.iter().find(|t| !names.insert(t.name.as_str())) {
            return Err(LimitError::DuplicateTarget { name: dup.name.clone() });
        }
        let too_many = LimitError::TooManyTargets { count: targets.len() };
        if targets.len() > MAX_TARGETS {
            return Err(too_many);
        }
        let count = u8::try_from(targets.len()).map_err(|_| too_many)?;
        let len = targets.iter().map(Target::encoded_len).fold(8 + 1, usize::saturating_add);
        let payload_len = u32::try_from(len)
            .ok()
            .filter(|&l| l <= MAX_PAYLOAD_LEN)
            .ok_or(LimitError::PayloadTooLarge { len })?;
        Ok(Announce { id, targets, count, payload_len })
    }

    #[must_use]
    pub fn id(&self) -> EntryId {
        self.id
    }

    #[must_use]
    pub fn targets(&self) -> &[Target] {
        &self.targets
    }

    /// The encoded payload's length, at most [`MAX_PAYLOAD_LEN`].
    pub(crate) fn payload_len(&self) -> u32 {
        self.payload_len
    }

    // Layout: id u64, target count u8, then per target: name length u8, name, size u64,
    // inline flag u8 (0 or 1), and if 1: inline length u32 and the bytes.
    pub(crate) fn encode_payload(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.id.0.to_be_bytes());
        out.push(self.count);
        for t in &self.targets {
            out.push(t.name.len);
            out.extend_from_slice(t.name.name.as_bytes());
            out.extend_from_slice(&t.size.to_be_bytes());
            match &t.inline {
                None => out.push(0),
                Some(i) => {
                    out.push(1);
                    out.extend_from_slice(&i.len.to_be_bytes());
                    out.extend_from_slice(&i.data);
                }
            }
        }
    }

    pub(crate) fn decode_payload(payload: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(payload);
        let id = EntryId(r.u64()?);
        let count = usize::from(r.u8()?);
        if count > MAX_TARGETS {
            return Err(LimitError::TooManyTargets { count }.into());
        }
        let mut targets = Vec::with_capacity(count);
        for _ in 0..count {
            let name_len = usize::from(r.u8()?);
            let name = TargetName::parse(r.take(name_len)?)?;
            let size = r.u64()?;
            let target = match r.u8()? {
                0 => Target::deferred(name, size),
                1 => {
                    let len = usize::try_from(r.u32()?).map_err(|_| DecodeError::Truncated)?;
                    if len > MAX_INLINE_LEN {
                        return Err(LimitError::InlineTooLarge { len }.into());
                    }
                    let data = r.take(len)?.to_vec();
                    if u64::try_from(data.len()).ok() != Some(size) {
                        return Err(LimitError::InlineSizeMismatch { size, inline_len: len }.into());
                    }
                    Target::inline(name, data)?
                }
                flag => return Err(DecodeError::BadFlag(flag)),
            };
            targets.push(target);
        }
        r.finish()?;
        Ok(Announce::new(id, targets)?)
    }
}
