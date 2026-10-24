use std::collections::HashSet;

use jackstay_clip::TargetName;

use crate::{Atom, Port};

/// Atoms the engine uses by name, interned once at start.
#[derive(Debug)]
pub(crate) struct Known {
    pub clipboard: Atom,
    pub primary: Atom,
    pub clipboard_manager: Atom,
    pub targets: Atom,
    pub multiple: Atom,
    pub timestamp: Atom,
    pub save_targets: Atom,
    pub incr: Atom,
    pub atom: Atom,
    pub atom_pair: Atom,
    pub integer: Atom,
    pub null: Atom,
    pub utf8_string: Atom,
    pub string: Atom,
    pub text: Atom,
    pub compound_text: Atom,
    pub text_plain: Atom,
    pub text_plain_utf8: Atom,
    pub clip_temporary: Atom,
    /// Targets never converted, on any path (docs/selection.md, "Targets the engine never
    /// converts").
    pub never_convert: HashSet<Atom>,
}

/// The longest atom list read from another client: `TARGETS`, a `MULTIPLE` request, a
/// `SAVE_TARGETS` list. 16,384 atoms is far beyond any real offer.
pub(crate) const MAX_ATOM_LIST_BYTES: usize = 64 * 1024;

/// Names of the never-convert list.
pub(crate) const NEVER_CONVERT: [&str; 12] = [
    "TARGETS",
    "MULTIPLE",
    "TIMESTAMP",
    "SAVE_TARGETS",
    "DELETE",
    "INSERT_PROPERTY",
    "INSERT_SELECTION",
    "LENGTH",
    "DRAWABLE",
    "BITMAP",
    "PIXMAP",
    "COLORMAP",
];

pub(crate) fn name(s: &str) -> Option<TargetName> {
    TargetName::from_latin1(s.as_bytes()).ok()
}

pub(crate) fn intern(port: &mut impl Port, s: &str) -> Atom {
    name(s).map_or(Atom::NONE, |n| port.intern(&n))
}

impl Known {
    pub(crate) fn intern(port: &mut impl Port) -> Self {
        let mut i = |s: &str| intern(port, s);
        let mut known = Known {
            clipboard: i("CLIPBOARD"),
            primary: i("PRIMARY"),
            clipboard_manager: i("CLIPBOARD_MANAGER"),
            targets: i("TARGETS"),
            multiple: i("MULTIPLE"),
            timestamp: i("TIMESTAMP"),
            save_targets: i("SAVE_TARGETS"),
            incr: i("INCR"),
            atom: i("ATOM"),
            atom_pair: i("ATOM_PAIR"),
            integer: i("INTEGER"),
            null: i("NULL"),
            utf8_string: i("UTF8_STRING"),
            string: i("STRING"),
            text: i("TEXT"),
            compound_text: i("COMPOUND_TEXT"),
            text_plain: i("text/plain"),
            text_plain_utf8: i("text/plain;charset=utf-8"),
            clip_temporary: i("CLIP_TEMPORARY"),
            never_convert: HashSet::new(),
        };
        known.never_convert = NEVER_CONVERT.iter().map(|s| intern(port, s)).collect();
        known
    }
}
