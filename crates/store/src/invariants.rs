//! What must hold of every input to each decoder, checked by the fuzz targets and by the replay of
//! their corpora (TS-16), so both test the same thing. Each returns whether the input was read, or
//! what failed.

use std::collections::HashSet;

use crate::clipman;
use crate::index::{SavedData, encode, parse};

/// Reading never fails on a whole record, never yields two entries with one id, an id at or above
/// `next-id`, or an entry `Content::new` would refuse (nothing kept, a name twice); writing what was
/// read reads back to it, whole; and an input read whole, with nothing dropped or raised, is the
/// one spelling of what it holds, so writing it gives back the input.
///
/// # Errors
/// What failed, for the fuzzer's report.
pub fn index(input: &[u8]) -> Result<bool, String> {
    let Ok(p) = parse(input) else { return Ok(false) };
    let mut ids = HashSet::new();
    for e in &p.index.entries {
        if !ids.insert(e.id) || e.id.get() >= p.index.next_id {
            return Err(format!("entry id {} repeats or is not below next-id", e.id.get()));
        }
        if !e.targets.iter().any(|t| matches!(t.data, SavedData::Kept { .. })) {
            return Err(format!("entry {} keeps nothing", e.id.get()));
        }
        let mut names = HashSet::new();
        if !e.targets.iter().all(|t| names.insert(&t.name)) {
            return Err(format!("entry {} names a target twice", e.id.get()));
        }
    }
    let written = encode(&p.index);
    let again = parse(&written).map_err(|e| format!("written index unreadable: {e}"))?;
    if again.index != p.index || !again.dropped.is_empty() || !again.complete {
        return Err("written index reads back differently".to_owned());
    }
    if p.complete && p.dropped.is_empty() && !p.next_id_raised && written != input {
        return Err("a whole index has another spelling".to_owned());
    }
    Ok(true)
}

/// `g_key_file_set_string_list`'s escaping of one item (`g_key_file_parse_string_as_value`):
/// leading spaces and tabs as `\s` and `\t` until another character ends the leading run, newlines,
/// returns, backslashes and the separator always.
#[must_use]
pub fn glib_escape(item: &str) -> String {
    let mut out = String::new();
    let mut leading = true;
    for c in item.chars() {
        match c {
            ' ' if leading => out.push_str("\\s"),
            '\t' if leading => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\\' => {
                out.push_str("\\\\");
                leading = false;
            }
            ';' => {
                out.push_str("\\;");
                leading = false;
            }
            c => {
                out.push(c);
                leading = false;
            }
        }
    }
    out
}

/// Reading never yields an empty item; and what is read, written as clipman writes it, reads back
/// the same, unless the list starts with a space `GLib` writes raw and strips again (vertical tab,
/// form feed), which clipman loses too.
///
/// # Errors
/// What failed, for the fuzzer's report.
pub fn clipman(input: &[u8]) -> Result<bool, String> {
    let Ok(items) = clipman::texts(input) else { return Ok(false) };
    if items.iter().any(String::is_empty) {
        return Err("an empty item".to_owned());
    }
    if items.first().is_some_and(|i| i.starts_with(['\u{b}', '\u{c}'])) {
        return Ok(true);
    }
    let list: String = items.iter().flat_map(|i| [glib_escape(i), ";".to_owned()]).collect();
    let again = clipman::texts(format!("[texts]\ntexts={list}\n").as_bytes());
    if again.as_ref() != Ok(&items) {
        let got = again.map(|a| a.len());
        return Err(format!("{} items read back differently ({got:?} items)", items.len()));
    }
    Ok(true)
}
