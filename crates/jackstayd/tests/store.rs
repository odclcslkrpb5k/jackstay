use std::fs;
use std::num::NonZeroUsize;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use jackstay_clip::{
    Content, Entry, EntryId, Format, History, Kept, Limits, NotKept, Source, Target, TargetData,
    TargetName,
};
use jackstay_store::BlobHash;
use jackstayd::store::{Loaded, Paths, Reply, Request, Snapshot, Store, load, run};

fn ok<T, E: std::fmt::Debug>(r: Result<T, E>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("expected Ok, got Err({e:?})"),
    }
}

/// A fresh state directory under the target directory.
fn state_dir() -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "store-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&dir);
    ok(fs::create_dir_all(&dir));
    dir
}

fn name(s: &str) -> TargetName {
    ok(TargetName::from_latin1(s.as_bytes()))
}

fn content(text: &str, image: Option<&[u8]>) -> Arc<Content> {
    let kept = |n: &str, b: &[u8]| Target {
        name: name(n),
        data: TargetData::Kept(ok(Kept::new(name(n), Format::Bits8, b.to_vec()))),
    };
    let mut targets = vec![kept("UTF8_STRING", text.as_bytes())];
    match image {
        Some(png) => targets.push(kept("image/png", png)),
        None => targets
            .push(Target { name: name("image/png"), data: TargetData::NotKept(NotKept::TooLarge) }),
    }
    Arc::new(ok(Content::new(targets)))
}

fn limits() -> Limits {
    Limits { max_entries: NonZeroUsize::new(100).unwrap_or(NonZeroUsize::MIN), max_bytes: 1 << 30 }
}

fn loaded(dir: &Path) -> Loaded {
    ok(load(&Paths::new(dir.to_path_buf()), limits(), 1 << 20))
}

fn history(items: &[(&str, Option<&[u8]>)]) -> History {
    let mut h = History::new(limits());
    for (i, (text, image)) in items.iter().enumerate().rev() {
        let source = Source::Local { owner_class: Some("Firefox".into()) };
        let _ = h.record(content(text, *image), source, 1_000 + u64::try_from(i).unwrap_or(0));
    }
    h
}

fn blobs(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = ok(fs::read_dir(dir.join("blobs")))
        .map(|e| ok(e).file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn same(a: &History, b: &History) {
    let view = |h: &History| -> Vec<(EntryId, Content, bool, u64, Source)> {
        h.entries()
            .map(|e| {
                (e.id(), (**e.content()).clone(), e.pinned(), e.copied_at_ms(), e.source().clone())
            })
            .collect()
    };
    assert!(view(a) == view(b), "histories differ");
    assert_eq!(a.next_id(), b.next_id());
}

#[test]
fn a_saved_history_loads_back_as_it_was() {
    let dir = state_dir();
    let mut h = history(&[("newest", Some(b"\x89PNG")), ("older", None)]);
    let older = h.entries().nth(1).map_or(EntryId::new(0), Entry::id);
    let _ = h.set_pinned(older, true);
    let mut store = Store::new(Paths::new(dir.clone()), &loaded(&dir));
    ok(store.save(&Snapshot::of(&h, true), false));
    let back = loaded(&dir);
    same(&back.history, &h);
    assert!(back.cleared);
    assert_eq!(back.report, jackstayd::store::LoadReport::default());
    for f in ["index"]
        .iter()
        .map(|f| dir.join(f))
        .chain(blobs(&dir).iter().map(|b| dir.join("blobs").join(b)))
    {
        let mode = ok(fs::metadata(&f)).permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{}", f.display());
    }
}

#[test]
fn equal_bytes_are_one_blob_and_unreferenced_blobs_go() {
    let dir = state_dir();
    let mut h = history(&[("same", Some(b"img")), ("other", Some(b"img"))]);
    let mut store = Store::new(Paths::new(dir.clone()), &loaded(&dir));
    ok(store.save(&Snapshot::of(&h, false), false));
    assert_eq!(blobs(&dir).len(), 3, "two texts and one shared image");
    let newest = h.newest().map_or(EntryId::new(0), Entry::id);
    let _ = h.delete(newest);
    ok(store.save(&Snapshot::of(&h, false), true));
    assert_eq!(
        blobs(&dir),
        {
            let mut v = vec![BlobHash::of(b"other").hex(), BlobHash::of(b"img").hex()];
            v.sort();
            v
        },
        "the deleted text's blob is gone, the shared image stays"
    );
    let _ = h.clear_unpinned();
    ok(store.save(&Snapshot::of(&h, false), false));
    assert_eq!(blobs(&dir), Vec::<String>::new());
    assert_eq!(loaded(&dir).history.entries().len(), 0);
}

#[test]
fn a_restart_deletes_what_the_previous_run_left_unreferenced() {
    let dir = state_dir();
    let mut h = history(&[("a", None), ("b", None)]);
    let mut store = Store::new(Paths::new(dir.clone()), &loaded(&dir));
    ok(store.save(&Snapshot::of(&h, false), false));
    // A new run that drops "b" (as the limits might) must delete its blob on the next save.
    let back = loaded(&dir);
    let b = h.entries().nth(1).map_or(EntryId::new(0), Entry::id);
    let _ = h.delete(b);
    let mut store = Store::new(Paths::new(dir.clone()), &back);
    ok(store.save(&Snapshot::of(&h, false), false));
    assert_eq!(blobs(&dir), vec![BlobHash::of(b"a").hex()]);
}

#[test]
fn an_entry_whose_blob_is_missing_is_dropped_and_the_rest_kept() {
    let dir = state_dir();
    let h = history(&[("kept", None), ("lost", None)]);
    let mut store = Store::new(Paths::new(dir.clone()), &loaded(&dir));
    ok(store.save(&Snapshot::of(&h, false), false));
    ok(fs::remove_file(dir.join("blobs").join(BlobHash::of(b"lost").hex())));
    let back = loaded(&dir);
    assert_eq!(back.history.entries().len(), 1);
    assert_eq!(back.report.dropped_entries, 1);
    assert_eq!(back.history.next_id(), h.next_id());
}

#[test]
fn an_unreadable_header_moves_everything_aside_and_deletes_nothing() {
    let dir = state_dir();
    let h = history(&[("precious", None)]);
    let mut store = Store::new(Paths::new(dir.clone()), &loaded(&dir));
    ok(store.save(&Snapshot::of(&h, false), false));
    ok(fs::write(dir.join("index"), b"garbage\n"));
    let back = loaded(&dir);
    assert!(back.report.moved_aside);
    assert_eq!(back.history.entries().len(), 0);
    let aside: Vec<String> = ok(fs::read_dir(&dir))
        .map(|e| ok(e).file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".broken-"))
        .collect();
    assert_eq!(aside.len(), 2, "{aside:?}");
    let blobs_aside = aside.iter().find(|n| n.starts_with("blobs.")).cloned().unwrap_or_default();
    assert!(dir.join(blobs_aside).join(BlobHash::of(b"precious").hex()).exists());
    // Clearing the history removes the set-aside copies too.
    let mut store = Store::new(Paths::new(dir.clone()), &back);
    ok(store.remove_leftovers(&Snapshot::of(&back.history, true)));
    let left: Vec<String> =
        ok(fs::read_dir(&dir)).map(|e| ok(e).file_name().to_string_lossy().into_owned()).collect();
    let mut left = left;
    left.sort();
    assert_eq!(left, ["blobs", "index"]);
}

#[test]
fn a_dropped_records_blobs_are_kept_until_the_history_is_cleared() {
    let dir = state_dir();
    let h = history(&[("good", None), ("damaged", None)]);
    let mut store = Store::new(Paths::new(dir.clone()), &loaded(&dir));
    ok(store.save(&Snapshot::of(&h, false), false));
    let index = String::from_utf8_lossy(&ok(fs::read(dir.join("index")))).into_owned();
    let damaged = BlobHash::of(b"damaged").hex();
    ok(fs::write(dir.join("index"), index.replace(&format!("8 {damaged}"), "7 x")));
    let back = loaded(&dir);
    assert_eq!((back.history.entries().len(), back.report.dropped_records), (1, 1));
    let mut store = Store::new(Paths::new(dir.clone()), &back);
    ok(store.save(&Snapshot::of(&back.history, false), false));
    assert!(blobs(&dir).contains(&damaged), "kept for recovery");
    ok(fs::write(dir.join("blobs").join(".tmp-stale"), b"half"));
    ok(store.remove_leftovers(&Snapshot::of(&back.history, false)));
    assert_eq!(blobs(&dir), vec![BlobHash::of(b"good").hex()]);
}

#[test]
fn writes_never_follow_a_planted_symbolic_link() {
    let dir = state_dir();
    let victim = dir.join("victim");
    ok(fs::write(&victim, b"untouched"));
    ok(std::os::unix::fs::symlink(&victim, dir.join("index.tmp")));
    ok(fs::create_dir_all(dir.join("blobs")));
    ok(std::os::unix::fs::symlink(
        &victim,
        dir.join("blobs").join(format!(".tmp-{}", BlobHash::of(b"x").hex())),
    ));
    let h = history(&[("x", None)]);
    let mut store = Store::new(Paths::new(dir.clone()), &loaded(&dir));
    ok(store.save(&Snapshot::of(&h, false), false));
    assert_eq!(ok(fs::read(&victim)), b"untouched");
    same(&loaded(&dir).history, &h);
    // Nor a read: an index that is a link is not followed.
    ok(fs::remove_file(dir.join("index")));
    ok(std::os::unix::fs::symlink(&victim, dir.join("index")));
    assert!(load(&Paths::new(dir.clone()), limits(), 1 << 20).is_err());
}

#[test]
fn verify_finds_blobs_changed_on_disk() {
    let dir = state_dir();
    let h = history(&[("fine", None), ("tampered", None)]);
    let mut store = Store::new(Paths::new(dir.clone()), &loaded(&dir));
    ok(store.save(&Snapshot::of(&h, false), false));
    ok(fs::write(dir.join("blobs").join(BlobHash::of(b"tampered").hex()), b"tampereD"));
    let mut back = loaded(&dir);
    let tampered = back.history.entries().nth(1).map(Entry::id);
    let to_verify = back.take_loaded();
    let store = Store::new(Paths::new(dir.clone()), &back);
    assert_eq!(store.verify(&to_verify, 1 << 20), tampered.into_iter().collect::<Vec<_>>());
}

#[test]
fn the_store_thread_coalesces_acknowledges_and_saves_on_stop() {
    let dir = state_dir();
    let mut back = loaded(&dir);
    let to_verify = back.take_loaded();
    let store = Store::new(Paths::new(dir.clone()), &back);
    let (requests, rx) = mpsc::channel();
    let (tx, replies) = mpsc::channel();
    let (mut wake, woken) = ok(std::os::unix::net::UnixStream::pair());
    let thread = std::thread::spawn(move || run(store, &to_verify, 1 << 20, &rx, &tx, &mut wake));
    let one = history(&[("one", None)]);
    let two = history(&[("two", None), ("one", None)]);
    ok(requests.send(Request::Save {
        snapshot: Snapshot::of(&one, false),
        urgent: false,
        ack: None,
    }));
    ok(requests.send(Request::Save {
        snapshot: Snapshot::of(&two, false),
        urgent: true,
        ack: Some(7),
    }));
    let reply = ok(replies.recv_timeout(Duration::from_secs(5)));
    assert_eq!(reply, Reply::Done { ack: 7, result: Ok(()) });
    let mut byte = [0u8; 1];
    ok(std::io::Read::read(&mut &woken, &mut byte));
    same(&loaded(&dir).history, &two);
    let three = history(&[("three", None)]);
    ok(requests.send(Request::Save {
        snapshot: Snapshot::of(&three, false),
        urgent: false,
        ack: None,
    }));
    ok(requests.send(Request::Stop));
    ok(thread.join().map_err(|_| "store thread panicked"));
    same(&loaded(&dir).history, &three);
    assert!(replies.try_recv().is_err(), "nothing failed");
}

/// Makes `path` a non-empty directory, so creating a file there fails.
fn block(path: &Path) {
    ok(fs::create_dir_all(path.join("x")));
}

fn unblock(path: &Path) {
    ok(fs::remove_dir_all(path));
}

fn has_blob(dir: &Path, bytes: &[u8]) -> bool {
    dir.join("blobs").join(BlobHash::of(bytes).hex()).exists()
}

#[test]
fn an_urgent_save_deletes_before_it_writes_so_a_failed_write_keeps_no_secret() {
    let dir = state_dir();
    let mut h = history(&[("password", None)]);
    let mut store = Store::new(Paths::new(dir.clone()), &loaded(&dir));
    ok(store.save(&Snapshot::of(&h, false), false));
    // A new large copy whose blob cannot be written (as on a full disk), then the password is
    // deleted: the clear must still remove the password.
    let password = h.newest().map_or(EntryId::new(0), Entry::id);
    let _ = h.record(content("big", Some(b"huge image")), Source::Local { owner_class: None }, 5);
    let _ = h.delete(password);
    block(&dir.join("blobs").join(format!(".tmp-{}", BlobHash::of(b"huge image").hex())));
    let err = store.save(&Snapshot::of(&h, true), true);
    assert_eq!(err.map_err(|e| e.step), Err(jackstayd::store::Step::WriteBlob));
    assert!(!has_blob(&dir, b"password"));
}

#[test]
fn when_the_index_cannot_be_written_only_an_urgent_save_has_deleted() {
    for urgent in [true, false] {
        let dir = state_dir();
        let mut h = history(&[("secret", None), ("kept", None)]);
        let mut store = Store::new(Paths::new(dir.clone()), &loaded(&dir));
        ok(store.save(&Snapshot::of(&h, false), false));
        let secret = h.newest().map_or(EntryId::new(0), Entry::id);
        let _ = h.delete(secret);
        block(&dir.join("index.tmp"));
        let err = store.save(&Snapshot::of(&h, true), urgent);
        assert_eq!(err.map_err(|e| e.step), Err(jackstayd::store::Step::WriteIndex));
        assert_eq!(has_blob(&dir, b"secret"), !urgent, "urgent: {urgent}");
        // The old index still names the secret: after a restart it counts as cleared if its blob is
        // gone, so nothing older is put back on the clipboard.
        unblock(&dir.join("index.tmp"));
        let back = loaded(&dir);
        assert_eq!(back.cleared, urgent);
        assert_eq!(back.history.entries().len(), if urgent { 1 } else { 2 });
    }
}

#[test]
fn an_updated_entry_is_saved_with_its_new_bytes() {
    let dir = state_dir();
    let mut h = History::new(limits());
    let src = Source::Local { owner_class: None };
    let id = h.record_partial(content("partial", None), src, 1).id;
    let mut store = Store::new(Paths::new(dir.clone()), &loaded(&dir));
    ok(store.save(&Snapshot::of(&h, false), false));
    let _ = h.update(id, content("partial, and the rest", Some(b"img")), true);
    ok(store.save(&Snapshot::of(&h, false), false));
    same(&loaded(&dir).history, &h);
    assert!(!has_blob(&dir, b"partial"));
    assert!(has_blob(&dir, b"partial, and the rest") && has_blob(&dir, b"img"));
}

#[test]
fn a_clean_start_sweeps_strays_and_an_unclean_one_keeps_them() {
    let dir = state_dir();
    let h = history(&[("a", None)]);
    let mut store = Store::new(Paths::new(dir.clone()), &loaded(&dir));
    ok(store.save(&Snapshot::of(&h, false), false));
    // A blob written just before a crash, and an interrupted write.
    ok(fs::write(dir.join("blobs").join(BlobHash::of(b"orphan").hex()), b"orphan"));
    ok(fs::write(dir.join("blobs").join(".tmp-x"), b"half"));
    let back = loaded(&dir);
    assert!(back.report.clean());
    assert!(!dir.join("blobs").join(".tmp-x").exists(), "an interrupted write goes at once");
    let mut store = Store::new(Paths::new(dir.clone()), &back);
    ok(store.save(&Snapshot::of(&back.history, false), false));
    assert!(!has_blob(&dir, b"orphan"));

    // With a dropped record the stray may be its blob: kept.
    ok(fs::write(dir.join("blobs").join(BlobHash::of(b"orphan").hex()), b"orphan"));
    let index = ok(fs::read_to_string(dir.join("index")));
    ok(fs::write(dir.join("index"), index.replace("end\n", "entry 99 0 0\nend\n")));
    let back = loaded(&dir);
    assert!(!back.report.clean());
    let mut store = Store::new(Paths::new(dir.clone()), &back);
    ok(store.save(&Snapshot::of(&back.history, false), false));
    assert!(has_blob(&dir, b"orphan"));
}

#[test]
fn the_blobs_directory_is_private() {
    let dir = state_dir();
    let _ = loaded(&dir);
    assert_eq!(ok(fs::metadata(dir.join("blobs"))).permissions().mode() & 0o777, 0o700);
}

struct StoreThread {
    requests: mpsc::Sender<Request>,
    replies: mpsc::Receiver<Reply>,
    thread: std::thread::JoinHandle<()>,
    _woken: std::os::unix::net::UnixStream,
}

fn start(dir: &Path) -> StoreThread {
    let mut back = loaded(dir);
    let to_verify = back.take_loaded();
    let store = Store::new(Paths::new(dir.to_path_buf()), &back);
    let (requests, rx) = mpsc::channel();
    let (tx, replies) = mpsc::channel();
    let (mut wake, woken) = ok(std::os::unix::net::UnixStream::pair());
    let thread = std::thread::spawn(move || run(store, &to_verify, 1 << 20, &rx, &tx, &mut wake));
    StoreThread { requests, replies, thread, _woken: woken }
}

fn save(t: &StoreThread, h: &History, urgent: bool, ack: Option<u64>) {
    ok(t.requests.send(Request::Save { snapshot: Snapshot::of(h, false), urgent, ack }));
}

fn newest_text(dir: &Path) -> Option<String> {
    let back = loaded(dir);
    let e = back.history.newest()?;
    Some(String::from_utf8_lossy(e.content().kept("UTF8_STRING")?.bytes()).into_owned())
}

#[test]
fn ordinary_saves_wait_out_the_second_and_only_the_latest_is_written() {
    let dir = state_dir();
    let t = start(&dir);
    save(&t, &history(&[("first", None)]), false, None);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(newest_text(&dir).as_deref(), Some("first"), "the first is written at once");
    save(&t, &history(&[("second", None)]), false, None);
    save(&t, &history(&[("third", None)]), false, None);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(newest_text(&dir).as_deref(), Some("first"), "the next waits out the second");
    std::thread::sleep(Duration::from_millis(1_200));
    assert_eq!(newest_text(&dir).as_deref(), Some("third"));
    assert!(!has_blob(&dir, b"second"), "a snapshot overtaken while waiting is never written");
    ok(t.requests.send(Request::Stop));
    ok(t.thread.join().map_err(|_| "panicked"));
}

#[test]
fn a_failed_save_is_reported_and_retried_until_it_succeeds() {
    let dir = state_dir();
    let t = start(&dir);
    block(&dir.join("index.tmp"));
    save(&t, &history(&[("x", None)]), true, Some(1));
    let reply = ok(t.replies.recv_timeout(Duration::from_secs(5)));
    assert!(matches!(reply, Reply::Done { ack: 1, result: Err(_) }), "{reply:?}");
    unblock(&dir.join("index.tmp"));
    assert_eq!(ok(t.replies.recv_timeout(Duration::from_secs(5))), Reply::Recovered);
    assert_eq!(newest_text(&dir).as_deref(), Some("x"));
    ok(t.requests.send(Request::Stop));
    ok(t.thread.join().map_err(|_| "panicked"));
}

#[test]
fn the_thread_clears_leftovers_and_reports_corrupt_blobs() {
    let dir = state_dir();
    let h = history(&[("fine", None), ("tampered", None)]);
    let mut store = Store::new(Paths::new(dir.clone()), &loaded(&dir));
    ok(store.save(&Snapshot::of(&h, false), false));
    ok(fs::write(dir.join("blobs").join(BlobHash::of(b"tampered").hex()), b"Tampered"));
    ok(fs::create_dir(dir.join("blobs.broken-1")));
    let t = start(&dir);
    let tampered = h.entries().nth(1).map(Entry::id).into_iter().collect::<Vec<_>>();
    assert_eq!(ok(t.replies.recv_timeout(Duration::from_secs(5))), Reply::Corrupt(tampered));
    let mut emptied = h;
    let _ = emptied.clear_unpinned();
    ok(t.requests.send(Request::ClearHistory { snapshot: Snapshot::of(&emptied, true), ack: 3 }));
    assert_eq!(
        ok(t.replies.recv_timeout(Duration::from_secs(5))),
        Reply::Done { ack: 3, result: Ok(()) }
    );
    assert!(!dir.join("blobs.broken-1").exists());
    assert_eq!(blobs(&dir), Vec::<String>::new());
    ok(t.requests.send(Request::Stop));
    ok(t.thread.join().map_err(|_| "panicked"));
}

#[test]
fn errors_name_the_step_and_the_kind_only() {
    let e = jackstayd::store::StoreError {
        step: jackstayd::store::Step::WriteIndex,
        kind: std::io::ErrorKind::StorageFull,
    };
    assert_eq!(e.to_string(), "WriteIndex failed: no storage space");
}

#[test]
fn a_blob_at_the_target_limit_loads_and_one_beyond_is_dropped() {
    let dir = state_dir();
    let h = history(&[("12345678", None)]);
    let mut store = Store::new(Paths::new(dir.clone()), &loaded(&dir));
    ok(store.save(&Snapshot::of(&h, false), false));
    let at = ok(load(&Paths::new(dir.clone()), limits(), 8));
    assert_eq!(at.history.entries().len(), 1);
    let below = ok(load(&Paths::new(dir.clone()), limits(), 7));
    assert_eq!((below.history.entries().len(), below.report.dropped_entries), (0, 1));
}

#[test]
fn a_failed_clear_history_removes_the_leftovers_when_retried() {
    let dir = state_dir();
    ok(fs::create_dir_all(dir.join("blobs.broken-1")));
    let t = start(&dir);
    block(&dir.join("index.tmp"));
    ok(t.requests
        .send(Request::ClearHistory { snapshot: Snapshot::of(&history(&[]), true), ack: 4 }));
    let reply = ok(t.replies.recv_timeout(Duration::from_secs(5)));
    assert!(matches!(reply, Reply::Done { ack: 4, result: Err(_) }), "{reply:?}");
    // The failed attempt's own sweep may already have removed the blocking index.tmp; another
    // leftover appears before the retry, which must sweep too.
    let _ = fs::remove_dir_all(dir.join("index.tmp"));
    ok(fs::create_dir_all(dir.join("blobs.broken-2")));
    assert_eq!(ok(t.replies.recv_timeout(Duration::from_secs(5))), Reply::Recovered);
    assert!(!dir.join("blobs.broken-1").exists() && !dir.join("blobs.broken-2").exists());
    ok(t.requests.send(Request::Stop));
    ok(t.thread.join().map_err(|_| "panicked"));
}

#[test]
fn corrupt_blobs_are_reported_once_across_verification_batches() {
    let dir = state_dir();
    let texts: Vec<String> = (0..20).map(|i| format!("entry {i}")).collect();
    let items: Vec<(&str, Option<&[u8]>)> = texts.iter().map(|t| (t.as_str(), None)).collect();
    let h = history(&items);
    let mut store = Store::new(Paths::new(dir.clone()), &loaded(&dir));
    ok(store.save(&Snapshot::of(&h, false), false));
    for t in [&texts[0], &texts[19]] {
        ok(fs::write(dir.join("blobs").join(BlobHash::of(t.as_bytes()).hex()), b"changed"));
    }
    let t = start(&dir);
    let ids: Vec<EntryId> = h.entries().map(Entry::id).collect();
    let expected = vec![ids[0], ids[19]];
    assert_eq!(ok(t.replies.recv_timeout(Duration::from_secs(5))), Reply::Corrupt(expected));
    assert!(t.replies.recv_timeout(Duration::from_millis(300)).is_err(), "one report");
    ok(t.requests.send(Request::Stop));
    ok(t.thread.join().map_err(|_| "panicked"));
}
