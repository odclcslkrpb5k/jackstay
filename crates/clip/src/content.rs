use std::fmt;
use std::sync::Arc;

/// The longest target or type name accepted, in bytes. The protocol allows 65535; real names are
/// under 100 bytes, and a longer one is skipped by the adapter rather than stored.
pub const MAX_NAME_LEN: usize = 4096;

/// The name of a selection target or property type, such as `UTF8_STRING` or `image/png`.
///
/// X atom names are Latin-1. Each byte is kept as the character with that code point, which is
/// lossless both ways, so any name an owner offers can be recorded and served back exactly. Spaces
/// and quotes occur in real names (some office suites use them); NUL never does.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TargetName(Arc<str>);

impl TargetName {
    /// # Errors
    /// [`ContentError::BadName`] if `latin1` is empty, longer than [`MAX_NAME_LEN`], or holds NUL.
    pub fn from_latin1(latin1: &[u8]) -> Result<Self, ContentError> {
        if latin1.is_empty() || latin1.len() > MAX_NAME_LEN || latin1.contains(&0) {
            return Err(ContentError::BadName);
        }
        Ok(TargetName(latin1.iter().map(|&b| char::from(b)).collect::<String>().into()))
    }

    /// The name's Latin-1 bytes, as X stores them.
    #[must_use]
    pub fn to_latin1(&self) -> Vec<u8> {
        // Every char came from one byte in from_latin1, so each fits in a u8.
        self.0.chars().filter_map(|c| u8::try_from(c).ok()).collect()
    }

    /// The name as text, for display and for matching well-known names.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TargetName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The unit size of a property's data, as X records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    Bits8,
    Bits16,
    Bits32,
}

impl Format {
    /// # Errors
    /// [`ContentError::BadFormat`] for anything but 8, 16 or 32.
    pub fn from_bits(bits: u8) -> Result<Self, ContentError> {
        match bits {
            8 => Ok(Format::Bits8),
            16 => Ok(Format::Bits16),
            32 => Ok(Format::Bits32),
            other => Err(ContentError::BadFormat(other)),
        }
    }

    #[must_use]
    pub fn bits(self) -> u8 {
        match self {
            Format::Bits8 => 8,
            Format::Bits16 => 16,
            Format::Bits32 => 32,
        }
    }

    #[must_use]
    pub fn unit_len(self) -> usize {
        match self {
            Format::Bits8 => 1,
            Format::Bits16 => 2,
            Format::Bits32 => 4,
        }
    }
}

/// The bytes of one target, exactly as its owner sent them, with the type and format to serve them
/// back under.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Kept {
    type_name: TargetName,
    format: Format,
    bytes: Arc<[u8]>,
}

impl Kept {
    /// # Errors
    /// [`ContentError::Misaligned`] if `bytes` is not a whole number of `format` units.
    pub fn new(
        type_name: TargetName,
        format: Format,
        bytes: impl Into<Arc<[u8]>>,
    ) -> Result<Self, ContentError> {
        let bytes = bytes.into();
        if bytes.len() % format.unit_len() != 0 {
            return Err(ContentError::Misaligned { format, len: bytes.len() });
        }
        Ok(Kept { type_name, format, bytes })
    }

    #[must_use]
    pub fn type_name(&self) -> &TargetName {
        &self.type_name
    }

    #[must_use]
    pub fn format(&self) -> Format {
        self.format
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Why a target the owner offered is not held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotKept {
    /// Jackstay fetches one target per kind of content (§5.2); this one was not that target.
    NotCanonical,
    /// Larger than the per-target limit.
    TooLarge,
    /// The owner refused to convert it, or did not answer in time.
    OwnerFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TargetData {
    Kept(Kept),
    NotKept(NotKept),
}

/// One target an owner offered, and what Jackstay holds of it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Target {
    pub name: TargetName,
    pub data: TargetData,
}

/// What one copy offered: every target, held or not. Never empty, names never repeat, and at least
/// one target is held, since an entry with nothing to paste is not an entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Content {
    targets: Vec<Target>,
}

impl Content {
    /// # Errors
    /// [`ContentError::NothingKept`] if no target is held, or [`ContentError::DuplicateTarget`].
    pub fn new(targets: Vec<Target>) -> Result<Self, ContentError> {
        if !targets.iter().any(|t| matches!(t.data, TargetData::Kept(_))) {
            return Err(ContentError::NothingKept);
        }
        let mut names: Vec<&TargetName> = targets.iter().map(|t| &t.name).collect();
        names.sort_unstable();
        if let Some(pair) = names.windows(2).find(|w| w.first() == w.last()) {
            let name = pair.first().map(|n| (*n).clone());
            return Err(ContentError::DuplicateTarget(name));
        }
        Ok(Content { targets })
    }

    #[must_use]
    pub fn targets(&self) -> &[Target] {
        &self.targets
    }

    /// The held bytes of `name`, if it was offered and is held.
    #[must_use]
    pub fn kept(&self, name: &str) -> Option<&Kept> {
        self.targets.iter().find(|t| t.name.as_str() == name).and_then(|t| match &t.data {
            TargetData::Kept(k) => Some(k),
            TargetData::NotKept(_) => None,
        })
    }

    /// Whether two copies hold the same thing: the same set of kept targets, with the same types,
    /// formats and bytes. Target order and the reasons a target was not kept do not matter.
    #[must_use]
    pub fn same_kept(&self, other: &Content) -> bool {
        self.kept_sorted() == other.kept_sorted()
    }

    fn kept_sorted(&self) -> Vec<(&TargetName, &Kept)> {
        let mut kept: Vec<(&TargetName, &Kept)> = self
            .targets
            .iter()
            .filter_map(|t| match &t.data {
                TargetData::Kept(k) => Some((&t.name, k)),
                TargetData::NotKept(_) => None,
            })
            .collect();
        kept.sort_by(|a, b| a.0.cmp(b.0));
        kept
    }

    /// Bytes held across every target.
    #[must_use]
    pub fn kept_len(&self) -> u64 {
        self.targets
            .iter()
            .filter_map(|t| match &t.data {
                TargetData::Kept(k) => u64::try_from(k.bytes.len()).ok(),
                TargetData::NotKept(_) => None,
            })
            .fold(0, u64::saturating_add)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentError {
    BadName,
    BadFormat(u8),
    Misaligned { format: Format, len: usize },
    NothingKept,
    DuplicateTarget(Option<TargetName>),
}

impl fmt::Display for ContentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContentError::BadName => write!(f, "target name is empty, too long, or holds NUL"),
            ContentError::BadFormat(bits) => write!(f, "property format {bits}"),
            ContentError::Misaligned { format, len } => {
                write!(f, "{len} bytes is not a whole number of {}-bit units", format.bits())
            }
            ContentError::NothingKept => write!(f, "no target held"),
            ContentError::DuplicateTarget(Some(name)) => write!(f, "duplicate target {name}"),
            ContentError::DuplicateTarget(None) => write!(f, "duplicate target"),
        }
    }
}

impl std::error::Error for ContentError {}
