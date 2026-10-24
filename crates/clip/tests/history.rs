use std::num::NonZeroUsize;
use std::sync::Arc;

use jackstay_clip::{
    Content, ContentError, Entry, EntryId, Format, History, Kept, Limits, MAX_NAME_LEN, NotKept,
    RestoreError, Source, Target, TargetData, TargetName,
};
use proptest::prelude::*;

fn ok<T, E: std::fmt::Debug>(r: Result<T, E>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("expected Ok, got Err({e:?})"),
    }
}

fn some<T>(o: Option<T>) -> T {
    match o {
        Some(v) => v,
        None => panic!("expected Some"),
    }
}

fn name(s: &str) -> TargetName {
    ok(TargetName::from_latin1(s.as_bytes()))
}

fn kept(target: &str, bytes: &[u8]) -> Target {
    let k = ok(Kept::new(name(target), Format::Bits8, bytes.to_vec()));
    Target { name: name(target), data: TargetData::Kept(k) }
}

fn text(s: &str) -> Arc<Content> {
    Arc::new(ok(Content::new(vec![kept("UTF8_STRING", s.as_bytes())])))
}

fn local() -> Source {
    Source::Local { owner_class: None }
}

fn limits(max_entries: usize, max_bytes: u64) -> Limits {
    Limits { max_entries: some(NonZeroUsize::new(max_entries)), max_bytes }
}

fn text_of(e: &Entry) -> String {
    String::from_utf8_lossy(some(e.content().kept("UTF8_STRING")).bytes()).into_owned()
}

fn texts(h: &History) -> Vec<String> {
    h.entries().map(text_of).collect()
}

fn ids(entries: &[Entry]) -> Vec<EntryId> {
    entries.iter().map(Entry::id).collect()
}

fn assert_none_evicted(entries: &[Entry]) {
    assert_eq!(ids(entries), Vec::<EntryId>::new());
}

/// Records a text copy that must evict nothing, and returns its id.
fn add(h: &mut History, s: &str, at: u64) -> EntryId {
    let r = h.record(text(s), local(), at);
    assert_none_evicted(&r.evicted);
    r.id
}

#[test]
fn names_are_lossless_latin1() {
    let libreoffice =
        br#"application/x-openoffice-embed-source-xml;windows_formatname="Star Embed Source (XML)""#;
    assert_eq!(ok(TargetName::from_latin1(libreoffice)).to_latin1(), libreoffice);
    let latin1 = [b'x', 0xE9, 0xFF, b'\t'];
    let n = ok(TargetName::from_latin1(&latin1));
    assert_eq!(n.to_latin1(), latin1);
    assert_eq!(n.as_str(), "x\u{e9}\u{ff}\t");
    assert_eq!(TargetName::from_latin1(b""), Err(ContentError::BadName));
    assert_eq!(TargetName::from_latin1(b"a\0b"), Err(ContentError::BadName));
    assert!(TargetName::from_latin1(&vec![b'x'; MAX_NAME_LEN]).is_ok());
    assert_eq!(TargetName::from_latin1(&vec![b'x'; MAX_NAME_LEN + 1]), Err(ContentError::BadName));
}

#[test]
fn kept_bytes_must_be_whole_units() {
    assert_eq!(
        Kept::new(name("ATOM"), Format::Bits32, vec![0; 6]),
        Err(ContentError::Misaligned { format: Format::Bits32, len: 6 })
    );
    assert!(Kept::new(name("ATOM"), Format::Bits32, vec![0; 8]).is_ok());
    assert!(Kept::new(name("ATOM"), Format::Bits16, vec![0; 3]).is_err());
    assert_eq!(Format::from_bits(7), Err(ContentError::BadFormat(7)));
    assert_eq!(some(Format::from_bits(16).ok()).bits(), 16);
    assert_eq!(some(Format::from_bits(32).ok()).unit_len(), 4);
}

#[test]
fn content_needs_something_kept_and_unique_names() {
    let offered = Target { name: name("image/png"), data: TargetData::NotKept(NotKept::TooLarge) };
    assert_eq!(Content::new(vec![offered.clone()]), Err(ContentError::NothingKept));
    let t = kept("UTF8_STRING", b"a");
    assert_eq!(
        Content::new(vec![t.clone(), t.clone()]),
        Err(ContentError::DuplicateTarget(Some(name("UTF8_STRING"))))
    );
    let c = ok(Content::new(vec![t, offered]));
    assert_eq!(c.kept_len(), 1);
    assert!(c.kept("image/png").is_none());
    let names: Vec<&str> = c.targets().iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["UTF8_STRING", "image/png"]);
}

#[test]
fn sameness_ignores_order_and_why_a_target_was_not_kept() {
    let a = ok(Content::new(vec![
        kept("UTF8_STRING", b"hi"),
        kept("text/html", b"<b>hi</b>"),
        Target { name: name("image/png"), data: TargetData::NotKept(NotKept::OwnerFailed) },
    ]));
    let b = ok(Content::new(vec![kept("text/html", b"<b>hi</b>"), kept("UTF8_STRING", b"hi")]));
    assert!(a.same_kept(&b));
    let c = ok(Content::new(vec![kept("UTF8_STRING", b"hi")]));
    assert!(!a.same_kept(&c));
    let d = ok(Content::new(vec![kept("UTF8_STRING", b"hi"), kept("text/html", b"<i>hi</i>")]));
    assert!(!a.same_kept(&d));
}

#[test]
fn copying_something_already_held_moves_it_to_the_front() {
    let mut h = History::new(limits(10, 1_000));
    let a = add(&mut h, "a", 1);
    add(&mut h, "b", 2);
    h.set_pinned(a, true);
    let firefox = Source::Local { owner_class: Some("Firefox".into()) };
    let again = h.record(text("a"), firefox.clone(), 3);
    assert_eq!(again.id, a);
    assert_none_evicted(&again.evicted);
    assert_eq!(texts(&h), ["a", "b"]);
    let e = some(h.get(a));
    assert_eq!((e.copied_at_ms(), e.source(), e.pinned()), (3, &firefox, true));
    assert_eq!(h.find(&text("b")), h.entries().nth(1).map(Entry::id));
    assert_eq!(h.find(&text("zzz")), None);
}

#[test]
fn entry_limit_counts_only_unpinned_entries() {
    let mut h = History::new(limits(2, 1_000));
    let pinned = add(&mut h, "p", 1);
    h.set_pinned(pinned, true);
    let one = add(&mut h, "1", 2);
    add(&mut h, "2", 3);
    let r = h.record(text("3"), local(), 4);
    assert_eq!(ids(&r.evicted), [one]);
    assert_eq!(texts(&h), ["3", "2", "p"]);
}

#[test]
fn byte_limit_leaves_out_the_newest_so_one_large_copy_keeps_the_history() {
    let mut h = History::new(limits(10, 5));
    add(&mut h, "abc", 1);
    add(&mut h, "de", 2);
    let big = h.record(text("a copy far larger than the budget"), local(), 3);
    assert_none_evicted(&big.evicted);
    assert_eq!(texts(&h), ["a copy far larger than the budget", "de", "abc"]);
    // Once it is no longer newest it counts. The oldest go first until the rest fit, so the large
    // copy goes last, after both small ones.
    let r = h.record(text("f"), local(), 4);
    let evicted: Vec<String> = r.evicted.iter().map(text_of).collect();
    assert_eq!(evicted, ["abc", "de", "a copy far larger than the budget"]);
    assert_eq!(texts(&h), ["f"]);
}

#[test]
fn promote_and_unpin_report_what_they_evict() {
    let mut h = History::new(limits(10, 5));
    let small = add(&mut h, "ab", 1);
    h.set_pinned(small, true);
    let big = add(&mut h, "much too long", 2);
    assert_eq!(texts(&h), ["much too long", "ab"]);
    let (content, evicted) = some(h.promote(small));
    assert!(content.same_kept(&text("ab")));
    assert_eq!(ids(&evicted), [big]);
    assert_eq!(texts(&h), ["ab"]);
    assert!(h.promote(EntryId::new(999)).is_none());

    let mut h = History::new(limits(1, 1_000));
    let a = add(&mut h, "a", 1);
    h.set_pinned(a, true);
    add(&mut h, "b", 2);
    assert_eq!(ids(&some(h.set_pinned(a, false))), [a]);
    assert!(h.set_pinned(a, true).is_none());
}

#[test]
fn delete_and_clear() {
    let mut h = History::new(limits(10, 1_000));
    let a = add(&mut h, "a", 1);
    let b = add(&mut h, "b", 2);
    let c = add(&mut h, "c", 3);
    h.set_pinned(b, true);
    assert_eq!(some(h.delete(a)).id(), a);
    assert!(h.delete(a).is_none());
    assert_eq!(ids(&h.clear_unpinned()), [c]);
    assert_eq!(texts(&h), ["b"]);
    assert_eq!(some(h.delete(b)).id(), b);
    assert_eq!(h.entries().len(), 0);
}

#[test]
fn restore_continues_ids_and_refuses_reuse() {
    let newer = Entry::new(EntryId::new(7), text("new"), local(), 2, false);
    let older = Entry::new(EntryId::new(3), text("old"), local(), 1, true);
    let saved = vec![newer.clone(), older];
    let (mut h, evicted) = ok(History::restore(limits(10, 1_000), saved.clone(), 8));
    assert_none_evicted(&evicted);
    assert_eq!(texts(&h), ["new", "old"]);
    assert_eq!(h.next_id(), 8);
    assert_eq!(h.record(text("next"), local(), 3).id, EntryId::new(8));
    assert_eq!(
        History::restore(limits(10, 1_000), saved.clone(), 7).err(),
        Some(RestoreError::NextIdTooLow { next_id: 7, highest: 7 })
    );
    assert_eq!(
        History::restore(limits(10, 1_000), vec![newer.clone(), newer], 9).err(),
        Some(RestoreError::DuplicateId(EntryId::new(7)))
    );
    let (h, evicted) = ok(History::restore(limits(1, 1_000), saved, 8));
    assert_none_evicted(&evicted);
    assert_eq!(texts(&h), ["new", "old"]);
}

#[test]
fn set_limits_reports_evictions() {
    let mut h = History::new(limits(10, 1_000));
    let a = add(&mut h, "a", 1);
    add(&mut h, "b", 2);
    assert_eq!(ids(&h.set_limits(limits(1, 1_000))), [a]);
}

#[test]
fn accessors() {
    let mut h = History::new(limits(10, 1_000));
    let source = Source::Local { owner_class: Some("Firefox".into()) };
    let id = h.record(text("a"), source.clone(), 42).id;
    let e = some(h.get(id));
    assert_eq!((e.id(), e.source(), e.copied_at_ms(), e.pinned()), (id, &source, 42, false));
    assert_eq!(EntryId::new(5).get(), 5);
    assert_eq!(some(h.newest()).id(), id);
    assert_eq!(name("TEXT").to_string(), "TEXT");
    assert_eq!(ContentError::NothingKept.to_string(), "no target held");
    assert_eq!(RestoreError::DuplicateId(EntryId::new(2)).to_string(), "entry id 2 appears twice");
}

#[test]
fn debug_shows_lengths_never_bytes() {
    let c = ok(Content::new(vec![kept("UTF8_STRING", b"hunter2-secret")]));
    let printed = format!("{c:?}");
    assert!(!printed.contains("hunter2"), "{printed}");
    assert!(!printed.contains("104, 117"), "the bytes as numbers: {printed}");
    assert!(printed.contains("len: 14"), "{printed}");
    let mut h = History::new(limits(10, 1_000));
    add(&mut h, "hunter2-secret", 1);
    assert!(!format!("{h:?}").contains("hunter2"));
}

#[test]
fn a_partial_copy_is_a_new_entry_until_complete() {
    let mut h = History::new(limits(10, 1_000));
    let old = add(&mut h, "same", 1);
    add(&mut h, "other", 2);
    let firefox = Source::Local { owner_class: Some("Firefox".into()) };
    let r = h.record_partial(text("same"), firefox.clone(), 3);
    assert_none_evicted(&r.evicted);
    assert_ne!(r.id, old, "a partial copy never merges");
    assert_eq!(texts(&h), ["same", "other", "same"]);
    let more = some(h.update(r.id, text("same, more"), false));
    assert_eq!(more.id, r.id);
    assert_none_evicted(&more.evicted);
    assert_eq!(texts(&h), ["same, more", "other", "same"]);
    let done = some(h.update(r.id, text("final"), true));
    assert_eq!(done.id, r.id, "nothing else holds it, so it stays itself");
    assert_eq!(texts(&h), ["final", "other", "same"]);
    let e = some(h.get(r.id));
    assert_eq!((e.copied_at_ms(), e.source()), (3, &firefox));
    assert!(h.update(EntryId::new(999), text("x"), true).is_none());
}

#[test]
fn completing_a_copy_of_an_older_entry_merges_into_it_in_place() {
    let mut h = History::new(limits(10, 1_000));
    let old = add(&mut h, "same", 1);
    h.set_pinned(old, true);
    add(&mut h, "other", 2);
    let firefox = Source::Local { owner_class: Some("Firefox".into()) };
    let partial = h.record_partial(text("sa"), firefox.clone(), 3).id;
    let r = some(h.update(partial, text("same"), true));
    assert_eq!(r.id, old, "the older entry keeps its id");
    assert_none_evicted(&r.evicted);
    assert_eq!(texts(&h), ["same", "other"]);
    assert!(h.get(partial).is_none());
    let e = some(h.get(old));
    assert_eq!((e.copied_at_ms(), e.source(), e.pinned()), (3, &firefox, true));

    // The partial entry's pin survives too, and a twin in front of it is handled.
    let mut h = History::new(limits(10, 1_000));
    let partial = h.record_partial(text("s"), local(), 1).id;
    h.set_pinned(partial, true);
    let newer = add(&mut h, "same", 2);
    add(&mut h, "newest", 3);
    let r = some(h.update(partial, text("same"), true));
    assert_eq!(r.id, newer);
    assert_eq!(texts(&h), ["newest", "same"]);
    let e = some(h.get(newer));
    assert_eq!((e.copied_at_ms(), e.pinned()), (1, true));
}

#[test]
fn updating_applies_the_limits() {
    let mut h = History::new(limits(10, 3));
    let old = add(&mut h, "ab", 1);
    let partial = h.record_partial(text("c"), local(), 2).id;
    h.promote(old);
    let r = some(h.update(partial, text("cdef"), false));
    assert_eq!(ids(&r.evicted), [partial], "no longer newest, it outgrew the budget");
}

#[test]
fn append_oldest_goes_to_the_back_and_skips_what_is_held() {
    let mut h = History::new(limits(2, 1_000));
    add(&mut h, "new", 2);
    let r = some(h.append_oldest(text("imported"), local(), 1));
    assert_none_evicted(&r.evicted);
    assert_eq!(texts(&h), ["new", "imported"]);
    assert_eq!(some(h.get(r.id)).copied_at_ms(), 1);
    assert!(h.append_oldest(text("new"), local(), 0).is_none());
    assert!(h.append_oldest(text("imported"), local(), 0).is_none());
    let full = some(h.append_oldest(text("older"), local(), 0));
    assert_eq!(ids(&full.evicted), [full.id], "a full history has no room at the back");
    assert_eq!(texts(&h), ["new", "imported"]);
    let mut empty = History::new(limits(2, 0));
    let only = some(empty.append_oldest(text("big"), local(), 0));
    assert_none_evicted(&only.evicted);
}

/// A deliberately plain model of the rules in `History`'s documentation, checked against it.
#[derive(Debug)]
struct Model {
    entries: Vec<(EntryId, String, bool)>, // newest first: id, text, pinned
    next_id: u64,
}

impl Model {
    fn enforce(&mut self, max_entries: usize, max_bytes: u64) -> Vec<EntryId> {
        let mut evicted = Vec::new();
        loop {
            let unpinned = self.entries.iter().filter(|e| !e.2).count();
            let bytes: usize =
                self.entries.iter().skip(1).filter(|e| !e.2).map(|e| e.1.len()).sum();
            if unpinned <= max_entries && u64::try_from(bytes).unwrap_or(u64::MAX) <= max_bytes {
                return evicted;
            }
            let Some(at) = (1..self.entries.len()).rev().find(|&i| !self.entries[i].2) else {
                return evicted;
            };
            evicted.push(self.entries.remove(at).0);
        }
    }

    fn record(&mut self, s: String) -> EntryId {
        if let Some(i) = self.entries.iter().position(|e| e.1 == s) {
            let e = self.entries.remove(i);
            let id = e.0;
            self.entries.insert(0, e);
            return id;
        }
        self.record_partial(s)
    }

    fn record_partial(&mut self, s: String) -> EntryId {
        let id = self.take_id();
        self.entries.insert(0, (id, s, false));
        id
    }

    fn update(&mut self, at: usize, s: String, complete: bool) -> EntryId {
        let id = self.entries[at].0;
        let twin = (0..self.entries.len()).find(|&j| j != at && self.entries[j].1 == s);
        match twin {
            Some(j) if complete => {
                let (twin_id, _, twin_pinned) = self.entries.remove(j);
                let at = if j < at { at - 1 } else { at };
                let pinned = twin_pinned || self.entries[at].2;
                self.entries[at] = (twin_id, s, pinned);
                twin_id
            }
            _ => {
                self.entries[at].1 = s;
                id
            }
        }
    }

    fn append_oldest(&mut self, s: String) -> Option<EntryId> {
        if self.entries.iter().any(|e| e.1 == s) {
            return None;
        }
        let id = self.take_id();
        self.entries.push((id, s, false));
        Some(id)
    }

    fn take_id(&mut self) -> EntryId {
        let id = EntryId::new(self.next_id);
        self.next_id += 1;
        id
    }
}

#[derive(Debug, Clone)]
enum Op {
    Record(u8),
    RecordPartial(u8),
    Update(usize, u8, bool),
    AppendOldest(u8),
    Pin(usize, bool),
    Promote(usize),
    Delete(usize),
    Clear,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (0u8..12).prop_map(Op::Record),
        2 => (0u8..12).prop_map(Op::RecordPartial),
        3 => (any::<usize>(), 0u8..12, any::<bool>()).prop_map(|(i, n, c)| Op::Update(i, n, c)),
        1 => (0u8..12).prop_map(Op::AppendOldest),
        1 => (any::<usize>(), any::<bool>()).prop_map(|(i, p)| Op::Pin(i, p)),
        1 => any::<usize>().prop_map(Op::Promote),
        1 => any::<usize>().prop_map(Op::Delete),
        1 => Just(Op::Clear),
    ]
}

proptest! {
    #[test]
    fn history_matches_the_model(
        max_entries in 1usize..5,
        max_bytes in 0u64..30,
        ops in proptest::collection::vec(op(), 0..80),
    ) {
        let mut h = History::new(limits(max_entries, max_bytes));
        let mut m = Model { entries: Vec::new(), next_id: 1 };
        for (t, op) in ops.into_iter().enumerate() {
            let pick = |i: usize, len: usize| (len > 0).then(|| i % len);
            let (got, want) = match op {
                Op::Record(n) => {
                    let s = "x".repeat(usize::from(n));
                    let r = h.record(text(&s), local(), u64::try_from(t).unwrap_or(0));
                    prop_assert_eq!(r.id, m.record(s));
                    (ids(&r.evicted), m.enforce(max_entries, max_bytes))
                }
                Op::RecordPartial(n) => {
                    let s = "x".repeat(usize::from(n));
                    let r = h.record_partial(text(&s), local(), u64::try_from(t).unwrap_or(0));
                    prop_assert_eq!(r.id, m.record_partial(s));
                    (ids(&r.evicted), m.enforce(max_entries, max_bytes))
                }
                Op::Update(i, n, complete) => match pick(i, m.entries.len()) {
                    Some(i) => {
                        let s = "x".repeat(usize::from(n));
                        let r = some(h.update(m.entries[i].0, text(&s), complete));
                        prop_assert_eq!(r.id, m.update(i, s, complete));
                        (ids(&r.evicted), m.enforce(max_entries, max_bytes))
                    }
                    None => (vec![], vec![]),
                },
                Op::AppendOldest(n) => {
                    let s = "x".repeat(usize::from(n));
                    let r = h.append_oldest(text(&s), local(), 0);
                    prop_assert_eq!(r.as_ref().map(|r| r.id), m.append_oldest(s));
                    match r {
                        Some(r) => (ids(&r.evicted), m.enforce(max_entries, max_bytes)),
                        None => (vec![], vec![]),
                    }
                }
                Op::Pin(i, p) => match pick(i, m.entries.len()) {
                    Some(i) => {
                        m.entries[i].2 = p;
                        let got = ids(&some(h.set_pinned(m.entries[i].0, p)));
                        (got, m.enforce(max_entries, max_bytes))
                    }
                    None => (vec![], vec![]),
                },
                Op::Promote(i) => match pick(i, m.entries.len()) {
                    Some(i) => {
                        let e = m.entries.remove(i);
                        let (_, evicted) = some(h.promote(e.0));
                        m.entries.insert(0, e);
                        (ids(&evicted), m.enforce(max_entries, max_bytes))
                    }
                    None => (vec![], vec![]),
                },
                Op::Delete(i) => match pick(i, m.entries.len()) {
                    Some(i) => {
                        let id = m.entries.remove(i).0;
                        prop_assert_eq!(h.delete(id).map(|e| e.id()), Some(id));
                        (vec![], vec![])
                    }
                    None => (vec![], vec![]),
                },
                Op::Clear => {
                    let removed = m.entries.iter().filter(|e| !e.2).map(|e| e.0).collect();
                    m.entries.retain(|e| e.2);
                    (ids(&h.clear_unpinned()), removed)
                }
            };
            prop_assert_eq!(got, want);
            let state: Vec<(EntryId, String, bool)> =
                h.entries().map(|e| (e.id(), text_of(e), e.pinned())).collect();
            prop_assert_eq!(&state, &m.entries);
        }
    }
}
