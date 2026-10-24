//! Serving a selection we own (docs/selection.md, "Serving").

use std::collections::HashMap;
use std::sync::Arc;

use jackstay_clip::{Content, Format, TargetData};

use crate::atoms::{Known, MAX_ATOM_LIST_BYTES};
use crate::text::{compound_text, from_latin1, latin1};
use crate::{Atom, Millis, Port, Read, Timestamp, Window};

/// How long a requestor may leave an INCR transfer idle before it is dropped.
pub(crate) const TRANSFER_IDLE_MS: u64 = 10_000;
/// The largest reply written in one property, and the INCR chunk size, unless the server's own
/// request limit is lower (docs/selection.md, "Serving").
pub(crate) const MAX_INCR_CHUNK: usize = 256 * 1024;

/// One reply's value.
#[derive(Debug, Clone)]
struct Value {
    type_: Atom,
    format: Format,
    data: Arc<[u8]>,
}

#[derive(Debug)]
struct Transfer {
    value: Value,
    offset: usize,
    end_sent: bool,
    deadline: Millis,
}

/// What we serve while we own a selection.
#[derive(Debug)]
pub(crate) struct Served {
    pub since: Timestamp,
    /// `None` when cleared: only `TARGETS` and `TIMESTAMP` are offered.
    content: Option<Arc<Content>>,
    /// Held targets by atom, with their type's atom, interned once.
    held: Vec<(Atom, Atom, usize)>,
    /// The held text, decoded, if any text target is held.
    text: Option<String>,
}

impl Served {
    pub(crate) fn new(
        port: &mut impl Port,
        content: Option<Arc<Content>>,
        since: Timestamp,
    ) -> Self {
        let mut held = Vec::new();
        let mut text = None;
        if let Some(c) = &content {
            for (i, t) in c.targets().iter().enumerate() {
                if let TargetData::Kept(k) = &t.data {
                    held.push((port.intern(&t.name), port.intern(k.type_name()), i));
                }
            }
            text = held_text(c);
        }
        Served { since, content, held, text }
    }

    /// Every target offered: held ones, text aliases, and the protocol's own. A cleared
    /// selection offers only `TARGETS` and `TIMESTAMP`.
    fn targets(&self, known: &Known) -> Vec<Atom> {
        if self.content.is_none() {
            return vec![known.targets, known.timestamp];
        }
        let mut out = vec![known.targets, known.multiple, known.timestamp];
        out.extend(self.held.iter().map(|h| h.0));
        if let Some(text) = &self.text {
            out.extend([known.utf8_string, known.text_plain_utf8, known.compound_text, known.text]);
            if latin1(text).is_some() {
                out.push(known.string);
            }
            if text.is_ascii() {
                out.push(known.text_plain);
            }
        }
        let mut seen = std::collections::HashSet::new();
        out.retain(|a| seen.insert(*a));
        out
    }

    /// The value of one target, or `None` if it is not offered.
    fn value(&self, known: &Known, target: Atom) -> Option<Value> {
        let bytes = |v: Vec<u8>| -> Arc<[u8]> { v.into() };
        if target == known.targets {
            let atoms: Vec<u8> =
                self.targets(known).iter().flat_map(|a| a.0.to_ne_bytes()).collect();
            return Some(Value { type_: known.atom, format: Format::Bits32, data: bytes(atoms) });
        }
        if target == known.timestamp {
            let data = bytes(self.since.0.to_ne_bytes().to_vec());
            return Some(Value { type_: known.integer, format: Format::Bits32, data });
        }
        let content = self.content.as_ref()?;
        if let Some(&(_, type_, i)) = self.held.iter().find(|h| h.0 == target)
            && let Some(TargetData::Kept(k)) = content.targets().get(i).map(|t| &t.data)
        {
            return Some(Value { type_, format: k.format(), data: k.bytes().into() });
        }
        let text = self.text.as_ref()?;
        let eight =
            |type_: Atom, v: Vec<u8>| Value { type_, format: Format::Bits8, data: bytes(v) };
        if target == known.utf8_string || target == known.text_plain_utf8 {
            Some(eight(known.utf8_string, text.as_bytes().to_vec()))
        } else if target == known.string {
            latin1(text).map(|l| eight(known.string, l))
        } else if target == known.text_plain {
            text.is_ascii().then(|| eight(known.text_plain, text.as_bytes().to_vec()))
        } else if target == known.compound_text {
            Some(eight(known.compound_text, compound_text(text)))
        } else if target == known.text {
            Some(match latin1(text) {
                Some(l) => eight(known.string, l),
                None => eight(known.compound_text, compound_text(text)),
            })
        } else {
            None
        }
    }
}

/// The text of a content: its `UTF8_STRING`, else `text/plain;charset=utf-8`, else `STRING`.
fn held_text(content: &Content) -> Option<String> {
    let utf8 = ["UTF8_STRING", "text/plain;charset=utf-8"]
        .iter()
        .find_map(|n| content.kept(n))
        .map(|k| String::from_utf8_lossy(k.bytes()).into_owned());
    utf8.or_else(|| content.kept("STRING").map(|k| from_latin1(k.bytes())))
}

/// Outgoing INCR transfers, keyed by (requestor, property).
#[derive(Debug)]
pub(crate) struct Transfers {
    live: HashMap<(Window, Atom), Transfer>,
    /// The largest value written in one property; also the chunk size.
    chunk: usize,
}

/// Where a reply goes. `motif` marks Motif's `CLIP_TEMPORARY` property, which cannot take INCR.
#[derive(Debug, Clone, Copy)]
struct Dest {
    requestor: Window,
    property: Atom,
    motif: bool,
}

pub(crate) struct Request {
    pub requestor: Window,
    pub selection: Atom,
    pub target: Atom,
    pub property: Option<Atom>,
    pub time: Timestamp,
}

impl Transfers {
    /// `max_property` is the most data the server takes in one `ChangeProperty`.
    pub(crate) fn new(max_property: usize) -> Self {
        Transfers { live: HashMap::new(), chunk: MAX_INCR_CHUNK.min(max_property).max(1) }
    }

    pub(crate) fn deadline(&self) -> Option<Millis> {
        self.live.values().map(|t| t.deadline).min()
    }

    /// Answers a request for a selection we own with `served`, or refuses it.
    pub(crate) fn answer(
        &mut self,
        port: &mut impl Port,
        known: &Known,
        served: Option<&Served>,
        req: &Request,
        now: Millis,
    ) {
        // An obsolete client's `None` property means "use the target as the property".
        let property = req.property.unwrap_or(req.target);
        let acceptable = served
            .filter(|s| req.time == Timestamp::CURRENT_TIME || !req.time.is_earlier_than(s.since));
        let ok = match acceptable {
            None => false,
            Some(served) if req.target == known.multiple && served.content.is_some() => {
                self.multiple(port, known, served, req.requestor, property, now)
            }
            Some(served) => match served.value(known, req.target) {
                None => false,
                Some(value) => {
                    let motif = property == known.clip_temporary;
                    let dest = Dest { requestor: req.requestor, property, motif };
                    self.write(port, known, dest, value, now)
                }
            },
        };
        let reply = ok.then_some(property);
        port.send_selection_notify(req.requestor, req.selection, req.target, reply, req.time);
    }

    /// Serves each pair of a `MULTIPLE` request; failed pairs get `None` as their property.
    fn multiple(
        &mut self,
        port: &mut impl Port,
        known: &Known,
        served: &Served,
        requestor: Window,
        property: Atom,
        now: Millis,
    ) -> bool {
        let Read::Value(list) = port.read_property(requestor, property, false, MAX_ATOM_LIST_BYTES)
        else {
            return false;
        };
        if list.format != Format::Bits32 || list.data.len() % 8 != 0 {
            return false;
        }
        let mut out = Vec::with_capacity(list.data.len());
        for pair in list.data.as_chunks::<8>().0 {
            let (Some(t), Some(p)) = (atom_at(pair, 0), atom_at(pair, 4)) else { return false };
            let served_pair = t != known.multiple
                && p != Atom::NONE
                && served.value(known, t).is_some_and(|v| {
                    let dest = Dest { requestor, property: p, motif: false };
                    self.write(port, known, dest, v, now)
                });
            out.extend_from_slice(&t.0.to_ne_bytes());
            out.extend_from_slice(&(if served_pair { p } else { Atom::NONE }).0.to_ne_bytes());
        }
        port.change_property(requestor, property, known.atom_pair, Format::Bits32, &out);
        true
    }

    /// Writes a value to the requestor's property, by INCR if it is large.
    fn write(
        &mut self,
        port: &mut impl Port,
        known: &Known,
        dest: Dest,
        value: Value,
        now: Millis,
    ) -> bool {
        let Dest { requestor, property, motif } = dest;
        // A newer request for the same property replaces an older transfer to it.
        let replaced = self.live.remove(&(requestor, property)).is_some();
        if value.data.len() <= self.chunk {
            port.change_property(requestor, property, value.type_, value.format, &value.data);
            if replaced {
                self.unwatch_if_idle(port, requestor);
            }
            return true;
        }
        if motif {
            return false;
        }
        let Ok(size) = u32::try_from(value.data.len()) else { return false };
        port.watch_window(requestor, true);
        port.change_property(requestor, property, known.incr, Format::Bits32, &size.to_ne_bytes());
        let transfer =
            Transfer { value, offset: 0, end_sent: false, deadline: now.after(TRANSFER_IDLE_MS) };
        self.live.insert((requestor, property), transfer);
        true
    }

    /// The requestor deleted a property: the next chunk of a transfer to it, if there is one.
    pub(crate) fn on_deleted(
        &mut self,
        port: &mut impl Port,
        window: Window,
        property: Atom,
        now: Millis,
    ) {
        let key = (window, property);
        let Some(t) = self.live.get_mut(&key) else { return };
        if t.end_sent {
            self.live.remove(&key);
            self.unwatch_if_idle(port, window);
            return;
        }
        let end = t.offset.saturating_add(self.chunk).min(t.value.data.len());
        let chunk = t.value.data.get(t.offset..end).unwrap_or_default();
        port.change_property(window, property, t.value.type_, t.value.format, chunk);
        t.end_sent = chunk.is_empty();
        t.offset = end;
        t.deadline = now.after(TRANSFER_IDLE_MS);
    }

    pub(crate) fn on_destroyed(&mut self, window: Window) {
        self.live.retain(|k, _| k.0 != window);
    }

    pub(crate) fn on_tick(&mut self, port: &mut impl Port, now: Millis) {
        let expired: Vec<Window> =
            self.live.iter().filter(|(_, t)| t.deadline <= now).map(|(k, _)| k.0).collect();
        self.live.retain(|_, t| t.deadline > now);
        for window in expired {
            self.unwatch_if_idle(port, window);
        }
    }

    fn unwatch_if_idle(&mut self, port: &mut impl Port, window: Window) {
        if !self.live.keys().any(|k| k.0 == window) {
            port.watch_window(window, false);
        }
    }
}

fn atom_at(bytes: &[u8], at: usize) -> Option<Atom> {
    let b = bytes.get(at..at.checked_add(4)?)?;
    Some(Atom(u32::from_ne_bytes(<[u8; 4]>::try_from(b).ok()?)))
}
