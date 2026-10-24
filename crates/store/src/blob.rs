use std::fmt;

use sha2::{Digest, Sha256};

/// The SHA-256 of one kept target's bytes, which names its blob file. Equal bytes share a blob.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlobHash([u8; 32]);

impl BlobHash {
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        BlobHash(Sha256::digest(bytes).into())
    }

    /// Parses the 64 lowercase hex digits of [`BlobHash::hex`], and nothing else.
    #[must_use]
    pub fn from_hex(hex: &[u8]) -> Option<Self> {
        let (pairs, rest) = hex.as_chunks::<2>();
        if !rest.is_empty() {
            return None;
        }
        let bytes: Vec<u8> = pairs
            .iter()
            .map(|&[hi, lo]| Some((digit(hi)? << 4) | digit(lo)?))
            .collect::<Option<_>>()?;
        Some(BlobHash(bytes.try_into().ok()?))
    }

    /// The file name: 64 lowercase hex digits.
    #[must_use]
    pub fn hex(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let digit = |n: u8| char::from(HEX.get(usize::from(n)).copied().unwrap_or(b'0'));
        self.0.iter().flat_map(|&b| [digit(b >> 4), digit(b & 0xF)]).collect()
    }
}

fn digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => c.checked_sub(b'0'),
        b'a'..=b'f' => c.checked_sub(b'a' - 10),
        _ => None,
    }
}

impl fmt::Debug for BlobHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BlobHash({})", self.hex())
    }
}
