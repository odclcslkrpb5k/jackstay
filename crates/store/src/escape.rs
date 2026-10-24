//! Fields in the index are single tokens: every byte outside a small safe set is written `%XX`
//! (uppercase hex), and the empty field is `%` alone. Decoding accepts only that canonical form, so
//! each value has exactly one spelling.

fn safe(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"-._~/+:,=@;".contains(&b)
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

pub(crate) fn escape(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "%".to_owned();
    }
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if safe(b) {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(char::from(HEX.get(usize::from(b >> 4)).copied().unwrap_or(b'0')));
            out.push(char::from(HEX.get(usize::from(b & 0xF)).copied().unwrap_or(b'0')));
        }
    }
    out
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => c.checked_sub(b'0'),
        b'A'..=b'F' => c.checked_sub(b'A' - 10),
        _ => None,
    }
}

/// The bytes a token spells, or `None` if it is not in canonical form.
pub(crate) fn unescape(token: &[u8]) -> Option<Vec<u8>> {
    if token == b"%" {
        return Some(Vec::new());
    }
    if token.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(token.len());
    let mut rest = token;
    while let Some((&b, tail)) = rest.split_first() {
        if b == b'%' {
            let (&[hi, lo], tail) = tail.split_first_chunk::<2>()?;
            let byte = (hex_digit(hi)? << 4) | hex_digit(lo)?;
            if safe(byte) {
                return None;
            }
            out.push(byte);
            rest = tail;
        } else if safe(b) {
            out.push(b);
            rest = tail;
        } else {
            return None;
        }
    }
    Some(out)
}
