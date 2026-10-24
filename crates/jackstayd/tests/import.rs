//! Importing clipman's history from its files.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use jackstay_clip::{History, Limits};
use jackstayd::import::{ImportError, Imported, import, remove};
use jackstayd::tracker::Tracker;

fn ok<T, E: std::fmt::Debug>(r: Result<T, E>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("{e:?}"),
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("import-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    ok(std::fs::create_dir_all(&dir));
    dir
}

fn tracker() -> Tracker {
    let limits = Limits {
        max_entries: NonZeroUsize::new(100).unwrap_or(NonZeroUsize::MIN),
        max_bytes: 1 << 30,
    };
    Tracker::new(History::new(limits), 10, false)
}

/// Each entry's first target and its bytes, newest first.
fn held(t: &Tracker) -> Vec<(String, Vec<u8>, u64)> {
    t.history()
        .entries()
        .map(|e| {
            let target = e.content().targets().first().map(|t| t.name.as_str().to_owned());
            let name = target.unwrap_or_default();
            let bytes = e.content().kept(&name).map(|k| k.bytes().to_vec()).unwrap_or_default();
            (name, bytes, e.copied_at_ms())
        })
        .collect()
}

fn set_mtime(path: &Path, secs: u64) {
    let file = ok(std::fs::File::options().write(true).open(path));
    ok(file.set_modified(UNIX_EPOCH + Duration::from_secs(secs)));
}

#[test]
fn texts_then_images_each_in_clipmans_order_with_their_files_times() {
    let dir = scratch("order");
    ok(std::fs::write(dir.join("textsrc"), "[texts]\ntexts=old;new;\n"));
    set_mtime(&dir.join("textsrc"), 1_000);
    for (n, bytes, at) in [(2, b"png2", 2_000), (10, b"pngA", 3_000)] {
        let path = dir.join(format!("image{n}.png"));
        ok(std::fs::write(&path, bytes));
        set_mtime(&path, at);
    }
    // Not clipman's.
    ok(std::fs::write(dir.join("image+3.png"), b"no"));
    ok(std::fs::write(dir.join("imagex.png"), b"no"));
    let mut t = tracker();
    assert_eq!(import(&mut t, &dir, 1 << 20), Ok(Imported { added: 4, skipped: 0 }));
    let text = |s: &str| ("UTF8_STRING".to_owned(), s.as_bytes().to_vec(), 1_000_000);
    let image = |b: &[u8], at| ("image/png".to_owned(), b.to_vec(), at);
    assert_eq!(
        held(&t),
        [text("new"), text("old"), image(b"pngA", 3_000_000), image(b"png2", 2_000_000)]
    );
    assert_eq!(import(&mut t, &dir, 1 << 20), Ok(Imported { added: 0, skipped: 4 }), "held");
}

#[test]
fn images_past_the_per_target_limit_are_skipped() {
    let dir = scratch("large");
    ok(std::fs::write(dir.join("image0.png"), b"1234"));
    ok(std::fs::write(dir.join("image1.png"), b"12345"));
    let mut t = tracker();
    assert_eq!(import(&mut t, &dir, 4), Ok(Imported { added: 1, skipped: 1 }));
    assert_eq!(held(&t).first().map(|(_, b, _)| b.clone()), Some(b"1234".to_vec()));
}

#[test]
fn files_beyond_64_mib_are_not_read() {
    let dir = scratch("huge");
    let file = ok(std::fs::File::create(dir.join("image0.png")));
    ok(file.set_len(64 << 20));
    let file = ok(std::fs::File::create(dir.join("image1.png")));
    ok(file.set_len((64 << 20) + 1));
    let mut t = tracker();
    assert_eq!(import(&mut t, &dir, 1 << 30), Ok(Imported { added: 1, skipped: 1 }));
    assert_eq!(held(&t).first().map(|(_, b, _)| b.len()), Some(64 << 20));
    let file = ok(std::fs::File::create(dir.join("textsrc")));
    ok(file.set_len((64 << 20) + 1));
    assert_eq!(import(&mut tracker(), &dir, 1 << 30), Err(ImportError::Unreadable));
}

#[test]
fn no_history_is_nothing_to_import_and_an_unreadable_one_is_refused() {
    let dir = scratch("none");
    assert_eq!(import(&mut tracker(), &dir.join("missing"), 1), Ok(Imported::default()));
    assert_eq!(import(&mut tracker(), &dir, 1), Ok(Imported::default()));
    ok(std::fs::create_dir(dir.join("textsrc")));
    assert_eq!(import(&mut tracker(), &dir, 1), Err(ImportError::Unreadable));
    ok(std::fs::remove_dir(dir.join("textsrc")));
    ok(std::fs::write(dir.join("textsrc"), "not a key file"));
    assert_eq!(import(&mut tracker(), &dir, 1), Err(ImportError::Unreadable));
}

#[test]
fn remove_deletes_clipmans_files_only() {
    let dir = scratch("remove");
    ok(std::fs::write(dir.join("image1.png"), b"p"));
    ok(std::fs::write(dir.join("other"), b"o"));
    assert!(remove(&dir).is_ok(), "no textsrc is not an error");
    assert!(!dir.join("image1.png").exists());
    assert!(dir.join("other").exists());
    ok(std::fs::write(dir.join("textsrc"), "[texts]\n"));
    assert!(remove(&dir).is_ok());
    assert!(!dir.join("textsrc").exists());
    ok(std::fs::create_dir_all(dir.join("textsrc").join("x")));
    assert!(remove(&dir).is_err(), "one that cannot be deleted is reported");
}
