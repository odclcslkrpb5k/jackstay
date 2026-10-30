//! Thumbnails (docs/daemon.md, "Thumbnails"): the confined helper, `jackstayd --thumbnail`; the
//! thread that runs it, with helpers that misbehave; and the daemon end to end, from an image
//! copied with xclip to the thumbnail a client is given.
mod support;

use std::io::{Cursor, Read as _, Write as _};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use jackstay_clip::{
    Content, EntryId, Format, History, Kept, Limits, Source, Target, TargetData, TargetName,
};
use jackstay_proto::control::{Request, Response, Thumb, Thumbnail};
use jackstayd::thumbnails::Thumbnails;
use support::*;

const HELPER: &str = env!("CARGO_BIN_EXE_jackstayd");

/// A `width` by `height` PNG, red on the left half, blue on the right.
fn png(width: u32, height: u32) -> Vec<u8> {
    let image = RgbaImage::from_fn(width, height, |x, _| {
        Rgba(if x < width / 2 { [255, 0, 0, 255] } else { [0, 0, 255, 255] })
    });
    let mut out = Cursor::new(Vec::new());
    ok(DynamicImage::ImageRgba8(image).write_to(&mut out, ImageFormat::Png));
    out.into_inner()
}

fn helper(input: &[u8]) -> (Option<i32>, Vec<u8>) {
    let mut child = ok(Command::new(HELPER)
        .arg("--thumbnail")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn());
    let mut stdin = child.stdin.take().unwrap_or_else(|| panic!("no stdin"));
    ok(stdin.write_all(input));
    drop(stdin);
    let out = ok(child.wait_with_output());
    (out.status.code(), out.stdout)
}

#[test]
fn the_helper_writes_a_thumbnail_or_says_there_is_none() {
    let (status, out) = helper(&png(300, 100));
    assert_eq!(status, Some(0));
    let t = ok(Thumbnail::decode(&out));
    assert_eq!((t.width(), t.height(), t.image_width(), t.image_height()), (128, 42, 300, 100));
    assert_eq!(helper(b"not an image"), (Some(1), vec![]));
    assert_eq!(helper(b""), (Some(1), vec![]));
    let png = png(300, 100);
    assert_eq!(helper(&png[..png.len() / 2]), (Some(1), vec![]));
}

/// The value of `name` in a `/proc/<pid>/limits`-like table: its soft limit.
fn limit(limits: &str, name: &str) -> Option<String> {
    let line = limits.lines().find(|l| l.starts_with(name))?;
    line.get(name.len()..)?.split_whitespace().next().map(str::to_owned)
}

#[test]
fn the_helper_confines_itself_before_it_reads() {
    let mut child = ok(Command::new(HELPER)
        .arg("--thumbnail")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn());
    let proc = PathBuf::from(format!("/proc/{}", child.id()));
    // Its input not yet closed: it is waiting to read, already confined. Its seccomp filter is the
    // last step (crates/thumbnail/tests/confine.rs proves what it stops), so every limit is set
    // once it shows; read before, a limit may not be yet. One filter more than this process has,
    // not filter mode alone: a container's runtime puts every process in it in filter mode.
    let filters = |status: &str| {
        let line = status.lines().find(|l| l.starts_with("Seccomp_filters:"))?;
        line.split_whitespace().nth(1)?.parse::<u32>().ok()
    };
    let own = filters(&ok(std::fs::read_to_string("/proc/self/status")));
    let confined = own.and_then(|n| n.checked_add(1));
    assert!(confined.is_some(), "no seccomp filter count");
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        let status = std::fs::read_to_string(proc.join("status")).unwrap_or_default();
        if filters(&status) == confined {
            break status;
        }
        assert!(Instant::now() < deadline, "never confined: {status}");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(status.lines().any(|l| l.split_whitespace().eq(["NoNewPrivs:", "1"])), "{status}");
    assert!(status.lines().any(|l| l.split_whitespace().eq(["Seccomp:", "2"])), "{status}");
    let limits = std::fs::read_to_string(proc.join("limits")).unwrap_or_default();
    for (name, value) in [
        ("Max address space", "1073741824"),
        ("Max cpu time", "5"),
        ("Max file size", "0"),
        ("Max core file size", "0"),
        ("Max processes", "0"),
        ("Max open files", "3"),
    ] {
        assert_eq!(limit(&limits, name).as_deref(), Some(value), "{name}");
    }
    drop(child.stdin.take());
    assert_eq!(ok(child.wait()).code(), Some(1));
}

fn name(s: &str) -> TargetName {
    ok(TargetName::from_latin1(s.as_bytes()))
}

fn content(targets: &[(&str, &[u8])]) -> Arc<Content> {
    let targets = targets
        .iter()
        .map(|(n, b)| Target {
            name: name(n),
            data: TargetData::Kept(ok(Kept::new(name(n), Format::Bits8, b.to_vec()))),
        })
        .collect();
    Arc::new(ok(Content::new(targets)))
}

fn history() -> History {
    let max_entries = NonZeroUsize::new(100).unwrap_or(NonZeroUsize::MIN);
    History::new(Limits { max_entries, max_bytes: 1 << 30 })
}

fn record(h: &mut History, targets: &[(&str, &[u8])]) -> EntryId {
    h.record(content(targets), Source::Local { owner_class: None }, 0).id
}

/// Waits for entry `id` to be done, and returns what it has.
fn settled(t: &mut Thumbnails, id: EntryId, within: Duration) -> Thumb {
    let deadline = Instant::now() + within;
    loop {
        t.receive();
        let thumb = t.get(id);
        if thumb != Thumb::Pending {
            return thumb;
        }
        assert!(Instant::now() < deadline, "still pending");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn each_image_entry_gets_one_others_none() {
    let mut t = ok(Thumbnails::start(PathBuf::from(HELPER)));
    let mut h = history();
    let text = record(&mut h, &[("UTF8_STRING", b"hello")]);
    let wide = png(300, 100);
    let image = record(&mut h, &[("UTF8_STRING", b"caption"), ("image/png", &wide)]);
    let broken = record(&mut h, &[("image/bmp", b"BM not really")]);
    t.sync(&h);
    assert_eq!(t.get(text), Thumb::Absent);
    assert_eq!(t.get(image), Thumb::Pending);
    let Thumb::Ready(made) = settled(&mut t, image, Duration::from_secs(20)) else {
        panic!("no thumbnail")
    };
    assert_eq!((made.width(), made.height()), (128, 42));
    assert_eq!(settled(&mut t, broken, Duration::from_secs(20)), Thumb::Absent);
    // Deleted: forgotten at once.
    let _ = h.delete(image);
    t.sync(&h);
    assert_eq!(t.get(image), Thumb::Absent);
    assert_eq!(t.get(EntryId::new(999)), Thumb::Absent);
}

#[test]
fn a_second_image_is_tried_when_the_first_does_not_decode() {
    let mut t = ok(Thumbnails::start(PathBuf::from(HELPER)));
    let mut h = history();
    let good = png(10, 10);
    let id = record(&mut h, &[("image/bmp", b"BM broken"), ("image/png", &good)]);
    t.sync(&h);
    assert!(matches!(settled(&mut t, id, Duration::from_secs(20)), Thumb::Ready(_)));
}

/// A stand-in helper, a shell script in `dir`.
fn fake_helper(dir: &Path, script: &str) -> PathBuf {
    let path = dir.join("helper");
    ok(std::fs::write(&path, format!("#!/bin/sh\n{script}\n")));
    ok(std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)));
    path
}

fn scratch(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("jackstay-thumbnail-{name}-{}", std::process::id()));
    ok(std::fs::create_dir_all(&dir));
    dir
}

#[test]
fn a_helper_that_hangs_is_killed_and_the_next_still_made() {
    let dir = scratch("hang");
    let marker = dir.join("started");
    // Hangs on the first image it is given, and makes the next one properly.
    let script = format!(
        "if [ -e {m} ]; then exec {HELPER} --thumbnail; fi; : > {m}; exec sleep 60",
        m = marker.display()
    );
    let mut t = ok(Thumbnails::start(fake_helper(&dir, &script)));
    let mut h = history();
    let first = record(&mut h, &[("image/png", &png(4, 4))]);
    t.sync(&h);
    let started = Instant::now();
    assert_eq!(settled(&mut t, first, Duration::from_secs(20)), Thumb::Absent);
    let took = started.elapsed();
    assert!(took >= Duration::from_secs(9), "given up after {took:?}, before its deadline");
    let second = record(&mut h, &[("image/png", &png(5, 5))]);
    t.sync(&h);
    assert!(matches!(settled(&mut t, second, Duration::from_secs(20)), Thumb::Ready(_)));
}

#[test]
fn a_helper_that_writes_too_much_or_lies_gives_no_thumbnail() {
    let dir = scratch("lies");
    let mut h = history();
    let id = record(&mut h, &[("image/png", &png(4, 4))]);
    for script in [
        // Endless output.
        "exec yes",
        // A thumbnail's header claiming more pixels than follow.
        r"printf '\000\002\000\002\000\000\000\002\000\000\000\002abcd'",
        // A good thumbnail, but a failing exit.
        &format!("{HELPER} --thumbnail; exit 2"),
        // Not a helper at all.
        "exit 0",
    ] {
        let mut t = ok(Thumbnails::start(fake_helper(&dir, script)));
        t.sync(&h);
        assert_eq!(settled(&mut t, id, Duration::from_secs(20)), Thumb::Absent, "{script}");
    }
}

#[test]
fn a_copied_image_has_a_thumbnail_for_clients() {
    let x = Xvfb::start();
    let home = Home::new();
    let _daemon = Daemon::start(&x, &home);
    let mut xclip = ok(x
        .command("xclip")
        .args(["-quiet", "-selection", "clipboard", "-target", "image/png", "-i"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn());
    let mut stdin = xclip.stdin.take().unwrap_or_else(|| panic!("no stdin"));
    ok(stdin.write_all(&png(300, 100)));
    drop(stdin);
    eventually("the image in the history", || ids(&home).len() == 1);
    let id = ids(&home)[0];
    let mut thumb = Thumb::Pending;
    let deadline = Instant::now() + Duration::from_secs(20);
    while thumb == Thumb::Pending && Instant::now() < deadline {
        match ask(&home, Request::Thumbnail { id }) {
            Response::Thumbnail { id: got, thumbnail } if got == id => thumb = thumbnail,
            other => panic!("not a thumbnail: {other:?}"),
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let Thumb::Ready(t) = thumb else { panic!("no thumbnail: {thumb:?}") };
    assert_eq!((t.width(), t.height(), t.image_width(), t.image_height()), (128, 42, 300, 100));
    assert_eq!(&t.rgba()[..4], &[255, 0, 0, 255]);
    assert_eq!(&t.rgba()[t.rgba().len() - 4..], &[0, 0, 255, 255]);
    assert_eq!(
        ask(&home, Request::Thumbnail { id: id + 1 }),
        Response::Thumbnail { id: id + 1, thumbnail: Thumb::Absent }
    );
    // Deleted with its entry.
    assert_eq!(ask(&home, Request::Delete { id }), Response::Ok);
    assert_eq!(
        ask(&home, Request::Thumbnail { id }),
        Response::Thumbnail { id, thumbnail: Thumb::Absent }
    );
    kill(xclip);
    let mut log = String::new();
    let _ = std::fs::File::open(home.root.join("daemon.log"))
        .and_then(|mut f| f.read_to_string(&mut log));
    assert!(!log.contains("thumbnails:"), "{log}");
}

/// Waits for `path` to exist: a stand-in helper has started.
fn started(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(Instant::now() < deadline, "the helper never started");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn an_entry_deleted_while_being_made_is_stopped_at_once() {
    let dir = scratch("deleted");
    let marker = dir.join("started");
    let script = format!(
        "if [ -e {m} ]; then exec {HELPER} --thumbnail; fi; : > {m}; exec sleep 60",
        m = marker.display()
    );
    let mut t = ok(Thumbnails::start(fake_helper(&dir, &script)));
    let mut h = history();
    let doomed = record(&mut h, &[("image/png", &png(4, 4))]);
    t.sync(&h);
    started(&marker);
    let _ = h.delete(doomed);
    let next = record(&mut h, &[("image/png", &png(6, 6))]);
    let asked = Instant::now();
    t.sync(&h);
    assert!(matches!(settled(&mut t, next, Duration::from_secs(20)), Thumb::Ready(_)));
    // Not after the hung helper's 10 s deadline: it was killed when its entry went.
    assert!(asked.elapsed() < Duration::from_secs(5), "took {:?}", asked.elapsed());
    assert_eq!(t.get(doomed), Thumb::Absent);
}

#[test]
fn an_entry_changed_while_being_made_gets_the_new_contents_thumbnail() {
    let dir = scratch("changed");
    let marker = dir.join("started");
    // Slow the first time, so the content changes while it runs.
    let script = format!(
        "if [ ! -e {m} ]; then : > {m}; sleep 1; fi; exec {HELPER} --thumbnail",
        m = marker.display()
    );
    let mut t = ok(Thumbnails::start(fake_helper(&dir, &script)));
    let mut h = history();
    let id = record(&mut h, &[("image/png", &png(4, 4))]);
    t.sync(&h);
    started(&marker);
    let grown = h.update(id, content(&[("image/png", &png(9, 3))]), true).map(|r| r.id);
    assert_eq!(grown, Some(id));
    t.sync(&h);
    let Thumb::Ready(made) = settled(&mut t, id, Duration::from_secs(20)) else {
        panic!("no thumbnail")
    };
    assert_eq!((made.image_width(), made.image_height()), (9, 3), "the new content's");
}

#[test]
fn thumbnails_are_made_in_the_historys_order() {
    let dir = scratch("order");
    let log = dir.join("order");
    // Notes the length of each image it is given, then makes its thumbnail.
    let script = format!(
        "cat > {d}/in.$$; wc -c < {d}/in.$$ >> {l}; exec {HELPER} --thumbnail < {d}/in.$$",
        d = dir.display(),
        l = log.display()
    );
    let (small, large) = (png(5, 5), png(60, 60));
    assert_ne!(small.len(), large.len());
    let mut h = history();
    let first = record(&mut h, &[("image/png", &small)]);
    let second = record(&mut h, &[("image/png", &large)]);
    // Copied again: back on top, with its old id, which is the lower.
    assert_eq!(record(&mut h, &[("image/png", &small)]), first);
    assert_eq!(h.entries().next().map(jackstay_clip::Entry::id), Some(first));
    let mut t = ok(Thumbnails::start(fake_helper(&dir, &script)));
    t.sync(&h);
    assert!(matches!(settled(&mut t, second, Duration::from_secs(20)), Thumb::Ready(_)));
    assert!(matches!(settled(&mut t, first, Duration::from_secs(20)), Thumb::Ready(_)));
    let order: Vec<usize> = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect();
    assert_eq!(order, [small.len(), large.len()], "the top entry first");
}
