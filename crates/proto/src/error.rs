use std::fmt;

/// Why a sequence of bytes is not a valid message. Callers branch on the variant, never on the
/// text (ER-8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The input ended inside a field.
    Truncated,
    /// A message was complete but bytes followed it inside its frame.
    TrailingBytes,
    /// A frame header announced a payload longer than [`crate::MAX_PAYLOAD_LEN`].
    FrameTooLarge { len: u32 },
    /// A frame carried a message kind this version does not know.
    UnknownKind(u8),
    /// A flag byte held a value other than 0 or 1.
    BadFlag(u8),
    /// A value broke one of the limits a valid message keeps.
    Limit(LimitError),
}

/// A limit that a message must keep, whether it is being decoded or built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LimitError {
    TooManyTargets {
        count: usize,
    },
    DuplicateTarget {
        name: crate::TargetName,
    },
    NoTargets,
    TargetNameEmpty,
    TargetNameTooLong {
        len: usize,
    },
    TargetNameNotPrintableAscii,
    InlineTooLarge {
        len: usize,
    },
    InlineSizeMismatch {
        size: u64,
        inline_len: usize,
    },
    PayloadTooLarge {
        len: usize,
    },
    TextTooLong {
        len: usize,
    },
    TextNotUtf8,
    TooManyEntries {
        count: usize,
    },
    /// A code or bit set this version does not know.
    UnknownCode {
        code: u8,
    },
}

impl From<LimitError> for DecodeError {
    fn from(e: LimitError) -> Self {
        DecodeError::Limit(e)
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Truncated => write!(f, "input ended inside a field"),
            DecodeError::TrailingBytes => write!(f, "bytes after the end of a message"),
            DecodeError::FrameTooLarge { len } => write!(f, "frame payload of {len} bytes"),
            DecodeError::UnknownKind(kind) => write!(f, "unknown message kind {kind}"),
            DecodeError::BadFlag(flag) => write!(f, "flag byte {flag}"),
            DecodeError::Limit(e) => write!(f, "{e}"),
        }
    }
}

impl fmt::Display for LimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LimitError::TooManyTargets { count } => write!(f, "{count} targets"),
            LimitError::NoTargets => write!(f, "no targets"),
            LimitError::DuplicateTarget { name } => write!(f, "duplicate target {}", name.as_str()),
            LimitError::TargetNameEmpty => write!(f, "empty target name"),
            LimitError::TargetNameTooLong { len } => write!(f, "target name of {len} bytes"),
            LimitError::TargetNameNotPrintableAscii => write!(f, "target name not printable ASCII"),
            LimitError::InlineTooLarge { len } => write!(f, "inline data of {len} bytes"),
            LimitError::InlineSizeMismatch { size, inline_len } => {
                write!(f, "target size {size} but {inline_len} inline bytes")
            }
            LimitError::PayloadTooLarge { len } => write!(f, "encoded payload of {len} bytes"),
            LimitError::TextTooLong { len } => write!(f, "text of {len} bytes"),
            LimitError::TextNotUtf8 => write!(f, "text not UTF-8"),
            LimitError::TooManyEntries { count } => write!(f, "{count} entries"),
            LimitError::UnknownCode { code } => write!(f, "unknown code {code}"),
        }
    }
}

impl std::error::Error for DecodeError {}
impl std::error::Error for LimitError {}
