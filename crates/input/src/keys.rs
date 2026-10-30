//! Held keys and buttons, and the translation of the primary's keycodes into this machine's
//! (docs/input.md, "Secondary").

use jackstay_proto::link::{KeyEntry, MIN_KEYCODE};

/// A set of keycodes (0–255) or buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Set([u64; 4]);

impl Set {
    fn word_bit(code: u8) -> (usize, u64) {
        (usize::from(code >> 6), 1u64 << (code & 63))
    }

    /// Adds `code`; whether it was not already there.
    pub fn insert(&mut self, code: u8) -> bool {
        let (w, bit) = Set::word_bit(code);
        let Some(word) = self.0.get_mut(w) else { return false };
        let new = *word & bit == 0;
        *word |= bit;
        new
    }

    /// Removes `code`; whether it was there.
    pub fn remove(&mut self, code: u8) -> bool {
        let (w, bit) = Set::word_bit(code);
        let Some(word) = self.0.get_mut(w) else { return false };
        let was = *word & bit != 0;
        *word &= !bit;
        was
    }

    #[must_use]
    pub fn contains(&self, code: u8) -> bool {
        let (w, bit) = Set::word_bit(code);
        self.0.get(w).is_some_and(|word| word & bit != 0)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|w| *w == 0)
    }

    /// The codes in the set, lowest first.
    pub fn iter(&self) -> impl Iterator<Item = u8> + '_ {
        (0..=u8::MAX).filter(|c| self.contains(*c))
    }

    /// A keycode bitmap as X and [`jackstay_proto::link::Message::Enter`] carry it: bit `k % 8`
    /// of byte `k / 8`.
    #[must_use]
    pub fn from_bitmap(bitmap: &[u8; 32]) -> Set {
        let mut set = Set::default();
        for code in 0..=u8::MAX {
            let byte = bitmap.get(usize::from(code >> 3)).copied().unwrap_or(0);
            if byte & (1 << (code & 7)) != 0 {
                set.insert(code);
            }
        }
        set
    }

    #[must_use]
    pub fn to_bitmap(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        for code in self.iter() {
            if let Some(byte) = out.get_mut(usize::from(code >> 3)) {
                *byte |= 1 << (code & 7);
            }
        }
        out
    }
}

/// One keycode's keysyms on a machine: group 1, levels 1 and 2 (0 is `NoSymbol`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Syms {
    pub level1: u32,
    pub level2: u32,
}

/// A machine's keymap: each keycode's [`Syms`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap {
    syms: Vec<Syms>,
}

impl Keymap {
    /// The keymap with `syms` for keycodes `first`, `first + 1`, …; others have no symbols.
    #[must_use]
    pub fn new(first: u8, syms: &[Syms]) -> Keymap {
        let mut all = vec![Syms::default(); 256];
        for (code, s) in (usize::from(first)..256).zip(syms) {
            if let Some(slot) = all.get_mut(code) {
                *slot = *s;
            }
        }
        Keymap { syms: all }
    }

    #[must_use]
    pub fn syms(&self, keycode: u8) -> Syms {
        self.syms.get(usize::from(keycode)).copied().unwrap_or_default()
    }

    /// The lowest keycode with `want`, by `same`.
    fn find(&self, want: Syms, same: impl Fn(Syms, Syms) -> bool) -> Option<u8> {
        (MIN_KEYCODE..=u8::MAX).find(|k| same(self.syms(*k), want))
    }
}

/// The primary's keycodes turned into this machine's: built once per keymap change on either
/// side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Translation {
    to: Vec<Option<u8>>,
    modifier: Set,
}

impl Translation {
    /// Before any `Keymap` arrives: no key, since a keycode's meaning here is unknown.
    #[must_use]
    pub fn none() -> Translation {
        Translation { to: vec![None; 256], modifier: Set::default() }
    }

    /// For each of the primary's keycodes, in order of preference: the same keycode when its
    /// keysyms are the same here; another keycode with the same keysyms; the same keycode when
    /// its first keysym is the same; another keycode with the same first keysym. A key with no
    /// keysyms there, one the primary did not list, and one matching nothing here are dropped:
    /// none of them is known to mean anything here.
    #[must_use]
    pub fn new(primary_first: u8, primary: &[KeyEntry], here: &Keymap) -> Translation {
        let mut to = vec![None; 256];
        let mut modifier = Set::default();
        for (code, e) in (primary_first..=u8::MAX).zip(primary) {
            let want = Syms { level1: e.level1, level2: e.level2 };
            let first_same = |a: Syms, b: Syms| a.level1 == b.level1;
            let mapped = if want == Syms::default() {
                None
            } else if here.syms(code) == want {
                Some(code)
            } else if let Some(k) = here.find(want, |a, b| a == b) {
                Some(k)
            } else if first_same(here.syms(code), want) {
                Some(code)
            } else {
                here.find(want, first_same)
            };
            if let Some(slot) = to.get_mut(usize::from(code)) {
                *slot = mapped;
            }
            if e.modifier {
                modifier.insert(code);
            }
        }
        Translation { to, modifier }
    }

    /// This machine's keycode for the primary's `keycode`, if any.
    #[must_use]
    pub fn key(&self, keycode: u8) -> Option<u8> {
        self.to.get(usize::from(keycode)).copied().flatten()
    }

    /// Whether the primary's `keycode` is a modifier there.
    #[must_use]
    pub fn is_modifier(&self, keycode: u8) -> bool {
        self.modifier.contains(keycode)
    }
}
