use std::num::NonZeroUsize;
use std::sync::Arc;

use jackstay_clip::{Content, EntryId, Format, History, Kept, Limits, Source, Target, TargetData};
use jackstay_selection::Report;
use jackstayd::tracker::{PinError, Tracker};
use proptest::prelude::*;

fn content(s: &str) -> Arc<Content> {
    let name = |n: &str| match jackstay_clip::TargetName::from_latin1(n.as_bytes()) {
        Ok(n) => n,
        Err(e) => panic!("{e}"),
    };
    let kept = match Kept::new(name("UTF8_STRING"), Format::Bits8, s.as_bytes().to_vec()) {
        Ok(k) => k,
        Err(e) => panic!("{e}"),
    };
    match Content::new(vec![Target { name: name("UTF8_STRING"), data: TargetData::Kept(kept) }]) {
        Ok(c) => Arc::new(c),
        Err(e) => panic!("{e}"),
    }
}

fn tracker(max_entries: usize) -> Tracker {
    let limits = Limits {
        max_entries: NonZeroUsize::new(max_entries).unwrap_or(NonZeroUsize::MIN),
        max_bytes: 1_000,
    };
    Tracker::new(History::new(limits), 2, false)
}

fn texts(t: &Tracker) -> Vec<String> {
    t.history()
        .entries()
        .map(|e| {
            let k = e.content().kept("UTF8_STRING").map(|k| k.bytes().to_vec()).unwrap_or_default();
            String::from_utf8_lossy(&k).into_owned()
        })
        .collect()
}

fn text_of(t: &Tracker, id: Option<EntryId>) -> Option<String> {
    let e = t.history().get(id?)?;
    let k = e.content().kept("UTF8_STRING")?;
    Some(String::from_utf8_lossy(k.bytes()).into_owned())
}

fn started(capture: u64) -> Report {
    Report::CaptureStarted { capture }
}

fn captured(capture: u64, s: &str, complete: bool) -> Report {
    Report::Captured { capture, content: content(s), source: Some("Firefox".into()), complete }
}

/// Applies reports in order; returns whether any changed what is saved.
fn apply(t: &mut Tracker, reports: &[Report]) -> bool {
    reports.iter().fold(false, |changed, r| t.on_report(r, 1_000) | changed)
}

/// Copies `s` as one complete capture and returns its entry.
fn copy(t: &mut Tracker, capture: u64, s: &str) -> EntryId {
    apply(t, &[started(capture), captured(capture, s, true)]);
    match t.current() {
        Some(id) => id,
        None => panic!("no current entry after copying {s}"),
    }
}

#[test]
fn a_progressive_capture_is_one_entry_and_the_current_one() {
    let mut t = tracker(10);
    assert!(!apply(&mut t, &[started(1)]));
    assert_eq!(t.current(), None);
    assert!(apply(&mut t, &[captured(1, "te", false)]));
    let id = t.current();
    assert!(apply(&mut t, &[captured(1, "text", false), captured(1, "text+image", true)]));
    assert_eq!(t.current(), id);
    assert_eq!(texts(&t), ["text+image"]);
    let e = t.history().get(id.unwrap_or(EntryId::new(0)));
    assert_eq!(
        e.map(|e| e.source().clone()),
        Some(Source::Local { owner_class: Some("Firefox".into()) })
    );
    // Reports after completion, or for another capture, change nothing.
    assert!(!apply(&mut t, &[captured(1, "late", true), captured(7, "stray", false)]));
    assert_eq!(texts(&t), ["text+image"]);
}

#[test]
fn completing_a_copy_of_an_older_entry_makes_that_entry_current() {
    let mut t = tracker(10);
    let old = copy(&mut t, 1, "same");
    copy(&mut t, 2, "other");
    apply(&mut t, &[started(3), captured(3, "sa", false), captured(3, "same", true)]);
    assert_eq!(t.current(), Some(old));
    assert_eq!(texts(&t), ["same", "other"]);
}

#[test]
fn a_capture_cut_short_is_completed_and_deduplicated() {
    {
        let end = Report::CaptureEnded { capture: 3 };
        let mut t = tracker(10);
        let old = copy(&mut t, 1, "same");
        copy(&mut t, 2, "other");
        apply(&mut t, &[started(3), captured(3, "same", false)]);
        assert_eq!(texts(&t), ["same", "other", "same"], "a duplicate while open");
        assert!(apply(&mut t, std::slice::from_ref(&end)));
        assert_eq!(texts(&t), ["same", "other"]);
        assert_eq!(t.current(), Some(old), "the merged entry is still on the clipboard");
    }
    // A new capture completes the open one first.
    let mut t = tracker(10);
    copy(&mut t, 1, "same");
    apply(&mut t, &[started(2), captured(2, "same", false), started(3)]);
    assert_eq!(texts(&t), ["same"]);
    assert_eq!(t.current(), None);
}

#[test]
fn clearing_mid_capture_spares_a_pinned_twin() {
    let mut t = tracker(10);
    let twin = copy(&mut t, 1, "same");
    assert_eq!(t.pin(twin, true), Ok(()));
    apply(&mut t, &[started(2), captured(2, "same", false)]);
    // The engine's clear: the capture ends (completing and merging it), then the claim.
    apply(&mut t, &[Report::CaptureEnded { capture: 2 }, Report::Claimed]);
    assert_eq!(t.current(), Some(twin));
    t.clipboard_cleared();
    assert_eq!(texts(&t), ["same"], "the pinned entry stays, and only once");
    assert!(t.cleared());
}

#[test]
fn an_owner_exiting_mid_capture_leaves_its_entry_current() {
    let mut t = tracker(10);
    apply(&mut t, &[started(1), captured(1, "half", false)]);
    let id = t.current();
    apply(&mut t, &[Report::CaptureEnded { capture: 1 }, Report::KeptAlive]);
    assert_eq!(t.current(), id);
    assert!(id.is_some());
}

#[test]
fn selecting_mid_capture_in_the_engines_order() {
    let mut t = tracker(10);
    let old = copy(&mut t, 1, "old");
    apply(&mut t, &[started(2), captured(2, "new", false)]);
    // The session promotes, then serves; serving ends the capture, then claims.
    assert!(t.select(old).is_some());
    apply(&mut t, &[Report::CaptureEnded { capture: 2 }, Report::Claimed]);
    t.served(old, true);
    assert_eq!(t.current(), Some(old));
    assert_eq!(texts(&t), ["old", "new"]);
}

#[test]
fn a_deleted_partial_entry_is_not_brought_back_by_later_reports() {
    let mut t = tracker(10);
    apply(&mut t, &[started(1), captured(1, "secret", false)]);
    let id = t.current().unwrap_or(EntryId::new(0));
    assert_eq!(t.delete(id), Some(true), "it was on the clipboard");
    assert!(!apply(&mut t, &[captured(1, "secret, all of it", true)]));
    assert!(!apply(&mut t, &[Report::CaptureEnded { capture: 1 }]));
    assert_eq!(texts(&t), Vec::<String>::new());
    assert_eq!(t.current(), None);
    assert_eq!(t.delete(id), None);
}

#[test]
fn an_evicted_partial_entry_is_gone_for_good() {
    let mut t = tracker(1);
    apply(&mut t, &[started(1), captured(1, "a", false)]);
    let a = t.current();
    let b = copy(&mut t, 2, "b");
    assert!(t.history().get(a.unwrap_or(b)).is_none());
    assert!(!apply(&mut t, &[captured(1, "a, late", true)]));
    assert_eq!(texts(&t), ["b"]);
}

#[test]
fn clearing_deletes_the_current_entry_unless_pinned_and_is_remembered() {
    let mut t = tracker(10);
    let keep = copy(&mut t, 1, "keep");
    let secret = copy(&mut t, 2, "secret");
    t.clipboard_cleared();
    assert_eq!(texts(&t), ["keep"]);
    assert!(t.history().get(secret).is_none());
    assert_eq!((t.cleared(), t.current()), (true, None));
    // The next copy is something on the clipboard again.
    copy(&mut t, 3, "new");
    assert!(!t.cleared());
    // A pinned current entry stays.
    assert_eq!(t.pin(keep, true), Ok(()));
    assert!(t.select(keep).is_some());
    t.served(keep, true);
    t.clipboard_cleared();
    assert!(t.history().get(keep).is_some());
    assert!(t.cleared());
}

#[test]
fn select_promotes_and_serving_makes_it_current() {
    let mut t = tracker(10);
    let a = copy(&mut t, 1, "a");
    copy(&mut t, 2, "b");
    t.clipboard_cleared();
    let served = t.select(a);
    assert_eq!(served.map(|c| c.kept_len()), Some(1));
    assert_eq!(texts(&t), ["a"], "b was current when cleared");
    assert!(!t.served(a, false), "another client owns the clipboard");
    assert_eq!(t.current(), None);
    assert!(t.served(a, true), "the cleared flag is saved");
    assert_eq!(t.current(), Some(a));
    assert!(!t.served(a, true));
    assert!(t.select(EntryId::new(99)).is_none());
    assert!(!t.served(EntryId::new(99), true));
}

#[test]
fn losing_the_clipboard_forgets_the_current_entry() {
    let mut t = tracker(10);
    copy(&mut t, 1, "a");
    assert!(!apply(&mut t, &[Report::KeptAlive]));
    assert!(t.current().is_some(), "kept alive, it is still ours");
    assert!(!apply(&mut t, &[Report::Lost]));
    assert_eq!(t.current(), None);
}

#[test]
fn pins_are_limited_and_need_an_entry() {
    let mut t = tracker(10);
    let a = copy(&mut t, 1, "a");
    let b = copy(&mut t, 2, "b");
    let c = copy(&mut t, 3, "c");
    assert_eq!(t.pin(a, true), Ok(()));
    assert_eq!(t.pin(b, true), Ok(()));
    assert_eq!(t.pin(b, true), Ok(()), "already pinned");
    assert_eq!(t.pin(c, true), Err(PinError::TooManyPinned));
    assert_eq!(t.pin(c, false), Ok(()));
    assert_eq!(t.pin(a, false), Ok(()));
    assert_eq!(t.pin(c, true), Ok(()));
    assert_eq!(t.pin(EntryId::new(99), true), Err(PinError::NoSuchEntry));
}

#[test]
fn clear_history_keeps_pinned_entries() {
    let mut t = tracker(10);
    let a = copy(&mut t, 1, "a");
    copy(&mut t, 2, "b");
    copy(&mut t, 3, "c");
    assert_eq!(t.pin(a, true), Ok(()));
    assert_eq!(t.clear_history(), 2);
    assert_eq!(texts(&t), ["a"]);
    assert_eq!(t.current(), None);
}

#[test]
fn imports_go_to_the_back_once() {
    let mut t = tracker(2);
    copy(&mut t, 1, "mine");
    let src = Source::Local { owner_class: None };
    assert!(t.import(content("theirs"), src.clone(), 5));
    assert!(!t.import(content("theirs"), src.clone(), 5), "already held");
    assert!(!t.import(content("older"), src, 4), "no room");
    assert_eq!(texts(&t), ["mine", "theirs"]);
}

#[derive(Debug, Clone)]
enum Op {
    Start,
    Report(u8, bool),
    End,
    Lost,
    KeptAlive,
    /// A report for a capture this many before the latest.
    Stale(u64),
    Select(usize),
    Clear,
    Delete(usize),
    Pin(usize, bool),
    ClearHistory,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        2 => Just(Op::Start),
        4 => (0u8..6, any::<bool>()).prop_map(|(n, c)| Op::Report(n, c)),
        1 => Just(Op::End),
        1 => Just(Op::Lost),
        1 => Just(Op::KeptAlive),
        1 => (0u64..3).prop_map(Op::Stale),
        1 => any::<usize>().prop_map(Op::Select),
        1 => Just(Op::Clear),
        1 => any::<usize>().prop_map(Op::Delete),
        1 => (any::<usize>(), any::<bool>()).prop_map(|(i, p)| Op::Pin(i, p)),
        1 => Just(Op::ClearHistory),
    ]
}

proptest! {
    /// Under any sequence: the current entry is in the history; content repeats only while a
    /// capture is open, and then at most once; a cleared clipboard has no current entry; and a
    /// completed capture's content is the current entry's.
    #[test]
    fn the_history_stays_consistent(ops in proptest::collection::vec(op(), 0..60)) {
        let mut t = tracker(4);
        let mut capture = 0u64;
        let mut open = false;
        for op in ops {
            let ids: Vec<EntryId> = t.history().entries().map(jackstay_clip::Entry::id).collect();
            let pick = |i: usize| (!ids.is_empty()).then(|| ids[i % ids.len()]);
            match op {
                Op::Start => {
                    capture += 1;
                    open = true;
                    t.on_report(&started(capture), 0);
                }
                Op::Report(n, complete) => {
                    let s = "x".repeat(usize::from(n) + 1);
                    let accepted = t.on_report(&captured(capture, &s, complete), 0);
                    if accepted && complete {
                        prop_assert_eq!(text_of(&t, t.current()), Some(s.clone()));
                    }
                    if complete {
                        open = false;
                    }
                }
                Op::End => {
                    open = false;
                    t.on_report(&Report::CaptureEnded { capture }, 0);
                }
                Op::Lost => { t.on_report(&Report::Lost, 0); }
                Op::KeptAlive => {
                    // The owner died: an open capture ends first; its entry stays current.
                    t.on_report(&Report::CaptureEnded { capture }, 0);
                    open = false;
                    t.on_report(&Report::KeptAlive, 0);
                }
                Op::Stale(back) => {
                    let old = capture.saturating_sub(back + 1);
                    prop_assert!(!t.on_report(&captured(old, "stale", true), 0) || old == capture);
                }
                Op::Select(i) => {
                    let ids: Vec<EntryId> = t.history().entries().map(jackstay_clip::Entry::id).collect();
                    if let Some(&id) = ids.get(i % ids.len().max(1)) {
                        // The engine's order: promote, then serving ends the capture, then claims.
                        prop_assert!(t.select(id).is_some());
                        t.on_report(&Report::CaptureEnded { capture }, 0);
                        open = false;
                        let still_there = t.history().get(id).is_some();
                        t.served(id, true);
                        if still_there {
                            prop_assert_eq!(t.current(), Some(id));
                        }
                    }
                }
                Op::Clear => {
                    t.on_report(&Report::CaptureEnded { capture }, 0);
                    open = false;
                    let on_clipboard = t.current().and_then(|id| t.history().get(id).map(|e| (id, e.pinned())));
                    t.clipboard_cleared();
                    prop_assert!(t.cleared() && t.current().is_none());
                    if let Some((id, pinned)) = on_clipboard {
                        prop_assert_eq!(t.history().get(id).is_some(), pinned, "only a pinned entry survives a clear");
                    }
                }
                Op::Delete(i) => if let Some(id) = pick(i) {
                    let was_current = t.delete(id);
                    prop_assert!(was_current.is_some());
                    if was_current == Some(true) {
                        // Deleting the entry on the clipboard clears it, as the session does.
                        t.on_report(&Report::CaptureEnded { capture }, 0);
                        open = false;
                        t.clipboard_cleared();
                    }
                },
                Op::Pin(i, p) => if let Some(id) = pick(i) { let _ = t.pin(id, p); },
                Op::ClearHistory => { t.clear_history(); }
            }
            if let Some(id) = t.current() {
                prop_assert!(t.history().get(id).is_some());
            }
            let all = texts(&t);
            let mut distinct = all.clone();
            distinct.sort();
            distinct.dedup();
            let repeats = all.len() - distinct.len();
            prop_assert!(repeats <= usize::from(open), "{repeats} repeats in {all:?}, open: {open}");
            prop_assert!(t.history().entries().filter(|e| e.pinned()).count() <= 2);
        }
    }
}
