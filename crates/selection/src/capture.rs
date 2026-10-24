//! Capture: fetching what an owner offers (docs/selection.md, "Capture").

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::sync::Arc;

use jackstay_clip::{Content, Format, Kept, NotKept, Target, TargetData, TargetName};

use crate::atoms::{Known, MAX_ATOM_LIST_BYTES};
use crate::pool::Pool;
use crate::{Atom, Millis, Port, Property, Read, Timestamp, Window};

/// How long an owner has to answer one conversion.
pub(crate) const ANSWER_MS: u64 = 10_000;
/// How long an owner may pause between INCR chunks.
pub(crate) const CHUNK_MS: u64 = 5_000;
/// How long a whole capture may take.
pub(crate) const CAPTURE_MS: u64 = 60_000;

/// Byte limits on what one capture keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub per_target: usize,
    pub per_entry: usize,
}

#[derive(Clone, PartialEq, Eq)]
enum Step {
    /// Waiting for the owner's `TARGETS`.
    Targets { property: Atom, deadline: Millis },
    /// Waiting for the owner to answer a conversion of `target`.
    Converting { target: (Atom, TargetName), property: Atom, deadline: Millis },
    /// Receiving `target` in INCR chunks.
    Incr {
        target: (Atom, TargetName),
        property: Atom,
        type_: Atom,
        format: Option<Format>,
        data: Vec<u8>,
        deadline: Millis,
    },
    /// Nothing outstanding.
    Done,
}

/// Shows the INCR data's length, never the data.
impl fmt::Debug for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::Targets { property, deadline } => f
                .debug_struct("Targets")
                .field("property", property)
                .field("deadline", deadline)
                .finish(),
            Step::Converting { target, property, deadline } => f
                .debug_struct("Converting")
                .field("target", target)
                .field("property", property)
                .field("deadline", deadline)
                .finish(),
            Step::Incr { target, property, type_, format, data, deadline } => f
                .debug_struct("Incr")
                .field("target", target)
                .field("property", property)
                .field("type_", type_)
                .field("format", format)
                .field("len", &data.len())
                .field("deadline", deadline)
                .finish(),
            Step::Done => f.write_str("Done"),
        }
    }
}

/// What a step of the capture produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Progress {
    /// Nothing the daemon needs to see.
    Nothing,
    /// The content so far; `complete` once nothing more will be fetched.
    Content { content: Arc<Content>, complete: bool },
    /// The capture ended with nothing kept.
    Failed,
}

/// One capture of one owner's offer.
#[derive(Debug)]
pub(crate) struct Capture {
    pub owner: Window,
    pub selection_time: Timestamp,
    pub source: Option<String>,
    selection: Atom,
    deadline: Millis,
    /// Whether `TARGETS` failed and the fallbacks are being tried.
    fallback: bool,
    offered: Vec<(Atom, TargetName)>,
    queue: VecDeque<(Atom, TargetName)>,
    results: Vec<Target>,
    kept_bytes: usize,
    step: Step,
    published: bool,
    /// Whether a target has finished since the last report.
    dirty: bool,
    /// What a `SAVE_TARGETS` asked for before `TARGETS` was answered.
    early_save: EarlySave,
}

/// A `SAVE_TARGETS` request kept until `TARGETS` is answered: everything offered, and/or these.
#[derive(Debug, Default)]
struct EarlySave {
    all: bool,
    listed: Vec<(Atom, TargetName)>,
}

/// Fallbacks for owners that do not answer `TARGETS`, tried in order until one is kept.
const FALLBACKS: [&str; 3] = ["UTF8_STRING", "STRING", "image/png"];

/// Text targets, best first; the first offered is fetched as the text.
const TEXT_PREFERENCE: [&str; 3] = ["UTF8_STRING", "text/plain;charset=utf-8", "STRING"];
/// Text targets the engine serves itself from whichever text it keeps (docs/selection.md,
/// "Serving"): fetching them as well would keep the same text two or three times over.
const TEXT_ALIASES: [&str; 2] = ["text/plain", "text/plain;charset=utf-8"];
/// Every text target the engine serves from a text it keeps. A `SAVE_TARGETS` asking for these
/// beside a text already kept or on its way is not obeyed for them: the owner would render the
/// same text again (GTK 3 takes over 20 s for 20 MiB of non-ASCII `text/plain`, while the exiting
/// application waits).
const SERVED_FROM_TEXT: [&str; 6] =
    ["UTF8_STRING", "text/plain;charset=utf-8", "STRING", "text/plain", "COMPOUND_TEXT", "TEXT"];
/// Fetched whenever offered.
const ALWAYS: [&str; 4] = [
    "x-special/gnome-copied-files",
    "chromium/x-web-custom-data",
    "image/svg+xml",
    "image/x-inkscape-svg",
];

/// Whether a `SelectionNotify` answers the conversion outstanding on `ours`, asked with
/// `asked_at`. Owners echo the request's time. A refusal (property `None`) must match it exactly,
/// or it may answer an older conversion of the same target; a reply naming our own property may
/// carry `CurrentTime` (docs/selection.md, "Properties for incoming transfers").
pub(crate) fn answers(
    ours: Atom,
    asked_at: Timestamp,
    property: Option<Atom>,
    time: Timestamp,
) -> bool {
    match property {
        None => time == asked_at,
        Some(p) => p == ours && (time == asked_at || time == Timestamp::CURRENT_TIME),
    }
}

pub(crate) struct Ctx<'a, P: Port> {
    pub port: &'a mut P,
    pub known: &'a Known,
    pub pool: &'a mut Pool,
    pub our_window: Window,
    pub limits: Limits,
    pub now: Millis,
}

impl Capture {
    /// Starts capturing `owner`'s offer by asking for its `TARGETS`.
    pub(crate) fn start<P: Port>(
        cx: &mut Ctx<'_, P>,
        selection: Atom,
        owner: Window,
        selection_time: Timestamp,
    ) -> Self {
        let source = cx.port.source_of(owner);
        let mut capture = Capture {
            owner,
            selection_time,
            source,
            selection,
            deadline: cx.now.after(CAPTURE_MS),
            fallback: false,
            offered: Vec::new(),
            queue: VecDeque::new(),
            results: Vec::new(),
            kept_bytes: 0,
            step: Step::Done,
            published: false,
            dirty: false,
            early_save: EarlySave::default(),
        };
        match cx.pool.take(cx.port, cx.now) {
            Some(property) => {
                cx.port.convert_selection(selection, cx.known.targets, property, selection_time);
                capture.step = Step::Targets { property, deadline: cx.now.after(ANSWER_MS) };
            }
            None => capture.start_fallbacks(cx),
        }
        capture
    }

    pub(crate) fn is_done(&self) -> bool {
        self.step == Step::Done
    }

    /// The earliest moment this capture needs attention.
    pub(crate) fn deadline(&self) -> Option<Millis> {
        let step = match &self.step {
            Step::Targets { deadline, .. }
            | Step::Converting { deadline, .. }
            | Step::Incr { deadline, .. } => Some(*deadline),
            Step::Done => None,
        };
        step.map(|d| d.min(self.deadline))
    }

    /// Adds targets to fetch, as `SAVE_TARGETS` asks (beyond the canonical set): `all` that are
    /// offered, and `listed`. Already fetched, already queued and never-convert targets are
    /// skipped. If `TARGETS` has not been answered yet, the request is kept and merged when it is.
    /// Restarts a finished capture.
    pub(crate) fn extend<P: Port>(
        &mut self,
        cx: &mut Ctx<'_, P>,
        all: bool,
        listed: &[(Atom, TargetName)],
    ) -> Progress {
        if matches!(self.step, Step::Targets { .. }) {
            self.early_save.all |= all;
            self.early_save.listed.extend_from_slice(listed);
            return Progress::Nothing;
        }
        let mut wanted = listed.to_vec();
        if all {
            wanted.extend_from_slice(&self.offered);
        }
        if self.has_text() {
            wanted.retain(|t| !SERVED_FROM_TEXT.contains(&t.1.as_str()));
        }
        self.enqueue(cx.known, &wanted);
        if self.step == Step::Done {
            self.deadline = cx.now.after(CAPTURE_MS);
            return self.next(cx);
        }
        Progress::Nothing
    }

    fn enqueue(&mut self, known: &Known, targets: &[(Atom, TargetName)]) {
        for t in targets {
            let fetched = self.results.iter().any(|r| r.name == t.1);
            let queued = self.queue.iter().any(|q| q.0 == t.0);
            let current = self.current_target().is_some_and(|c| c.0 == t.0);
            if !fetched && !queued && !current && !known.never_convert.contains(&t.0) {
                self.queue.push_back(t.clone());
            }
        }
    }

    /// Gives up on whatever is outstanding; its property may still be written to by the owner.
    pub(crate) fn abandon(&mut self, pool: &mut Pool, now: Millis) {
        if let Some(property) = self.outstanding_property() {
            pool.quarantine(property, now);
        }
        self.queue.clear();
        self.step = Step::Done;
    }

    /// The owner answered a conversion.
    pub(crate) fn on_notify<P: Port>(
        &mut self,
        cx: &mut Ctx<'_, P>,
        target: Atom,
        property: Option<Atom>,
        time: Timestamp,
    ) -> Progress {
        match self.step.clone() {
            Step::Targets { property: ours, .. }
                if target == cx.known.targets
                    && answers(ours, self.selection_time, property, time) =>
            {
                self.step = Step::Done;
                let offered = if property.is_some() {
                    Self::read_targets(cx, ours)
                } else {
                    cx.pool.release(ours);
                    None
                };
                match offered {
                    Some(offered) => self.plan(cx, offered),
                    None => self.start_fallbacks(cx),
                }
                self.next(cx)
            }
            Step::Converting { target: wanted, property: ours, .. }
                if target == wanted.0 && answers(ours, self.selection_time, property, time) =>
            {
                if property.is_some() {
                    self.read_target(cx, wanted, ours);
                } else {
                    cx.pool.release(ours);
                    self.finish_target(wanted.1, TargetData::NotKept(NotKept::OwnerFailed));
                }
                self.next(cx)
            }
            _ => Progress::Nothing,
        }
    }

    /// A property on our window got a new value: the next INCR chunk, if we are receiving one.
    pub(crate) fn on_new_value<P: Port>(&mut self, cx: &mut Ctx<'_, P>, atom: Atom) -> Progress {
        let Step::Incr { target, property, type_, format, mut data, .. } = self.step.clone() else {
            return Progress::Nothing;
        };
        if atom != property {
            return Progress::Nothing;
        }
        let room = self.room(cx.limits).saturating_sub(data.len());
        match cx.port.read_property(cx.our_window, property, true, room) {
            Read::Missing => Progress::Nothing,
            Read::TooLarge { .. } => {
                cx.port.delete_property(cx.our_window, property);
                cx.pool.quarantine(property, cx.now);
                self.finish_target(target.1, TargetData::NotKept(NotKept::TooLarge));
                self.next(cx)
            }
            Read::Value(chunk) if chunk.data.is_empty() => {
                cx.pool.release(property);
                let format = format.unwrap_or(chunk.format);
                self.keep(cx, target.1, type_, format, data);
                self.next(cx)
            }
            Read::Value(chunk) => {
                if format.is_some_and(|f| f != chunk.format) {
                    cx.pool.quarantine(property, cx.now);
                    self.finish_target(target.1, TargetData::NotKept(NotKept::OwnerFailed));
                    return self.next(cx);
                }
                data.extend_from_slice(&chunk.data);
                self.step = Step::Incr {
                    target,
                    property,
                    type_: if format.is_none() { chunk.type_ } else { type_ },
                    format: Some(chunk.format),
                    data,
                    deadline: cx.now.after(CHUNK_MS),
                };
                Progress::Nothing
            }
        }
    }

    /// Time passed; a deadline may have been missed.
    pub(crate) fn on_tick<P: Port>(&mut self, cx: &mut Ctx<'_, P>) -> Progress {
        let Some(due) = self.deadline() else { return Progress::Nothing };
        if cx.now < due {
            return Progress::Nothing;
        }
        if cx.now >= self.deadline {
            // The whole capture ran out of time: keep what is held, give up the rest.
            let pending: Vec<TargetName> = self
                .current_target()
                .map(|t| t.1.clone())
                .into_iter()
                .chain(self.queue.iter().map(|t| t.1.clone()))
                .collect();
            self.abandon(cx.pool, cx.now);
            for name in pending {
                self.finish_target(name, TargetData::NotKept(NotKept::OwnerFailed));
            }
            return self.publish(true);
        }
        let current = self.current_target().cloned();
        if let Some(property) = self.outstanding_property() {
            cx.pool.quarantine(property, cx.now);
        }
        match current {
            None => self.start_fallbacks(cx), // TARGETS went unanswered
            Some(t) => self.finish_target(t.1, TargetData::NotKept(NotKept::OwnerFailed)),
        }
        self.step = Step::Done;
        self.next(cx)
    }

    /// Whether a text is kept, being fetched, or queued.
    fn has_text(&self) -> bool {
        let is_text = |n: &TargetName| TEXT_PREFERENCE.contains(&n.as_str());
        self.results.iter().any(|r| is_text(&r.name) && matches!(r.data, TargetData::Kept(_)))
            || self.queue.iter().any(|q| is_text(&q.1))
            || self.current_target().is_some_and(|c| is_text(&c.1))
    }

    fn current_target(&self) -> Option<&(Atom, TargetName)> {
        match &self.step {
            Step::Converting { target, .. } | Step::Incr { target, .. } => Some(target),
            Step::Targets { .. } | Step::Done => None,
        }
    }

    fn outstanding_property(&self) -> Option<Atom> {
        match &self.step {
            Step::Targets { property, .. }
            | Step::Converting { property, .. }
            | Step::Incr { property, .. } => Some(*property),
            Step::Done => None,
        }
    }

    /// Bytes this entry may still keep.
    fn room(&self, limits: Limits) -> usize {
        limits.per_target.min(limits.per_entry.saturating_sub(self.kept_bytes))
    }

    fn read_targets<P: Port>(
        cx: &mut Ctx<'_, P>,
        property: Atom,
    ) -> Option<Vec<(Atom, TargetName)>> {
        let read = cx.port.read_property(cx.our_window, property, true, MAX_ATOM_LIST_BYTES);
        // An INCR reply: our read-and-delete told the owner to start sending, so the property is
        // in doubt until the owner gives up. Otherwise the owner is finished with it.
        if matches!(&read, Read::Value(p) if p.type_ == cx.known.incr) {
            cx.pool.quarantine(property, cx.now);
            return None;
        }
        cx.pool.release(property);
        let Read::Value(Property { type_, format: Format::Bits32, data }) = read else {
            return None;
        };
        if type_ != cx.known.atom && type_ != cx.known.targets {
            return None;
        }
        let mut seen = HashSet::new();
        let offered = data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| Atom(u32::from_ne_bytes(*b)))
            .filter(|&a| a != Atom::NONE && seen.insert(a))
            .filter_map(|a| cx.port.atom_name(a).map(|n| (a, n)))
            .collect();
        Some(offered)
    }

    /// Chooses what to fetch from what is offered (docs/selection.md, "Capture", step 4).
    fn plan<P: Port>(&mut self, cx: &mut Ctx<'_, P>, offered: Vec<(Atom, TargetName)>) {
        let usable = |t: &&(Atom, TargetName)| !cx.known.never_convert.contains(&t.0);
        let by_name = |n: &str| offered.iter().filter(usable).find(|t| t.1.as_str() == n).cloned();
        let mut queue: Vec<(Atom, TargetName)> = Vec::new();
        let text = TEXT_PREFERENCE.iter().find_map(|n| by_name(n));
        queue.extend(text.clone());
        for t in offered.iter().filter(usable) {
            let name = t.1.as_str();
            let alias = text.is_some() && TEXT_ALIASES.contains(&name);
            let other_text = name.starts_with("text/") && Some(t) != text.as_ref() && !alias;
            if other_text || ALWAYS.contains(&name) {
                queue.push(t.clone());
            }
        }
        let raster = by_name("image/png").or_else(|| {
            offered
                .iter()
                .filter(usable)
                .find(|t| t.1.as_str().starts_with("image/") && !ALWAYS.contains(&t.1.as_str()))
                .cloned()
        });
        queue.extend(raster);
        let mut seen = HashSet::new();
        self.queue = queue.into_iter().filter(|t| seen.insert(t.0)).collect();
        self.offered = offered;
        let EarlySave { all, listed: mut saved } = std::mem::take(&mut self.early_save);
        if all {
            saved.extend_from_slice(&self.offered);
        }
        if self.has_text() {
            saved.retain(|t| !SERVED_FROM_TEXT.contains(&t.1.as_str()));
        }
        self.enqueue(cx.known, &saved);
    }

    fn start_fallbacks<P: Port>(&mut self, cx: &mut Ctx<'_, P>) {
        self.fallback = true;
        self.queue = FALLBACKS
            .iter()
            .filter_map(|n| crate::atoms::name(n))
            .map(|n| (cx.port.intern(&n), n))
            .collect();
        self.step = Step::Done;
    }

    fn read_target<P: Port>(
        &mut self,
        cx: &mut Ctx<'_, P>,
        target: (Atom, TargetName),
        property: Atom,
    ) {
        let room = self.room(cx.limits);
        match cx.port.read_property(cx.our_window, property, true, room) {
            Read::Missing => {
                cx.pool.release(property);
                self.finish_target(target.1, TargetData::NotKept(NotKept::OwnerFailed));
            }
            Read::TooLarge { .. } => {
                cx.port.delete_property(cx.our_window, property);
                cx.pool.release(property);
                self.finish_target(target.1, TargetData::NotKept(NotKept::TooLarge));
            }
            Read::Value(p) if p.type_ == cx.known.incr => {
                let announced = p
                    .data
                    .get(..4)
                    .and_then(|b| <[u8; 4]>::try_from(b).ok())
                    .map(u32::from_ne_bytes)
                    .and_then(|n| usize::try_from(n).ok());
                if announced.is_some_and(|n| n > room) {
                    // Our read already told the owner to start; it may write chunks we ignore.
                    cx.pool.quarantine(property, cx.now);
                    self.finish_target(target.1, TargetData::NotKept(NotKept::TooLarge));
                    return;
                }
                self.step = Step::Incr {
                    target,
                    property,
                    type_: Atom::NONE,
                    format: None,
                    data: Vec::new(),
                    deadline: cx.now.after(CHUNK_MS),
                };
            }
            Read::Value(p) => {
                cx.pool.release(property);
                self.keep(cx, target.1, p.type_, p.format, p.data);
            }
        }
    }

    fn keep<P: Port>(
        &mut self,
        cx: &mut Ctx<'_, P>,
        name: TargetName,
        type_: Atom,
        format: Format,
        data: Vec<u8>,
    ) {
        let type_name = cx.port.atom_name(type_).unwrap_or_else(|| name.clone());
        let len = data.len();
        let data = match Kept::new(type_name, format, data) {
            Ok(kept) => {
                self.kept_bytes = self.kept_bytes.saturating_add(len);
                TargetData::Kept(kept)
            }
            Err(_) => TargetData::NotKept(NotKept::OwnerFailed),
        };
        self.finish_target(name, data);
        if self.fallback && self.kept_bytes > 0 {
            self.queue.clear();
        }
    }

    fn finish_target(&mut self, name: TargetName, data: TargetData) {
        self.results.retain(|r| r.name != name);
        self.results.push(Target { name, data });
        self.step = Step::Done;
        self.dirty = true;
    }

    /// Starts the next queued conversion, or reports that the capture is complete.
    fn next<P: Port>(&mut self, cx: &mut Ctx<'_, P>) -> Progress {
        let updated = std::mem::take(&mut self.dirty);
        while self.step == Step::Done {
            let Some(target) = self.queue.pop_front() else {
                return self.publish(true);
            };
            if self.room(cx.limits) == 0 {
                self.finish_target(target.1, TargetData::NotKept(NotKept::TooLarge));
                continue;
            }
            let Some(property) = cx.pool.take(cx.port, cx.now) else {
                self.finish_target(target.1, TargetData::NotKept(NotKept::OwnerFailed));
                continue;
            };
            cx.port.convert_selection(self.selection, target.0, property, self.selection_time);
            self.step = Step::Converting { target, property, deadline: cx.now.after(ANSWER_MS) };
        }
        if updated { self.publish(false) } else { Progress::Nothing }
    }

    /// The content so far, with every offered target not fetched recorded as not kept.
    fn publish(&mut self, complete: bool) -> Progress {
        self.dirty = false;
        let mut targets = self.results.clone();
        if !self.fallback {
            for (_, name) in &self.offered {
                if !targets.iter().any(|t| &t.name == name) {
                    let data = TargetData::NotKept(NotKept::NotCanonical);
                    targets.push(Target { name: name.clone(), data });
                }
            }
        }
        match Content::new(targets) {
            Ok(content) => {
                self.published = true;
                Progress::Content { content: Arc::new(content), complete }
            }
            Err(_) if complete && !self.published => Progress::Failed,
            Err(_) => Progress::Nothing,
        }
    }
}
