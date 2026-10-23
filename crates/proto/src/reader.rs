use crate::DecodeError;

/// A cursor over untrusted bytes. Every read is checked against the bytes remaining, so a length
/// field can never send a read past the end (the defect class of CVE-2026-41476, where inner
/// lengths were trusted once the outer size had been checked).
#[derive(Debug)]
pub struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    #[must_use]
    pub fn new(bytes: &'a [u8]) -> Self {
        Reader { rest: bytes }
    }

    /// # Errors
    /// [`DecodeError::Truncated`] if fewer than `n` bytes remain.
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let (head, tail) = self.rest.split_at_checked(n).ok_or(DecodeError::Truncated)?;
        self.rest = tail;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        self.take(N)?.try_into().map_err(|_| DecodeError::Truncated)
    }

    /// # Errors
    /// [`DecodeError::Truncated`] at the end of input.
    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(u8::from_be_bytes(self.array()?))
    }

    /// # Errors
    /// [`DecodeError::Truncated`] if fewer than 4 bytes remain.
    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    /// # Errors
    /// [`DecodeError::Truncated`] if fewer than 8 bytes remain.
    pub fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    /// Ends the read: a message must use every byte of its frame.
    ///
    /// # Errors
    /// [`DecodeError::TrailingBytes`] if any bytes remain.
    pub fn finish(self) -> Result<(), DecodeError> {
        if self.rest.is_empty() { Ok(()) } else { Err(DecodeError::TrailingBytes) }
    }
}
