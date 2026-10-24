//! Replays the fuzzers' saved corpora as ordinary tests (TS-16), checking every input that once
//! reached new code against the same invariants the fuzzers check, without nightly.

use std::path::Path;

use jackstay_store::invariants;

/// Checks every input of `target`'s corpus; returns how many there were and how many were read.
fn replay(target: &str, check: fn(&[u8]) -> Result<bool, String>) -> (usize, usize) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus").join(target);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) => panic!("corpus {} unreadable: {e}", dir.display()),
    };
    let (mut inputs, mut read) = (0, 0);
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(e) => panic!("corpus entry unreadable: {e}"),
        };
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) => panic!("{} unreadable: {e}", path.display()),
        };
        match check(&bytes) {
            Ok(was_read) => read += usize::from(was_read),
            Err(e) => panic!("{}: {e}", path.display()),
        }
        inputs += 1;
    }
    (inputs, read)
}

// A corpus gone missing or empty would pass while replaying nothing (TS-6), and one where the
// reader regressed to refusing everything would still replay every file.

#[test]
fn every_saved_index_input_holds_the_invariants() {
    let (inputs, read) = replay("index", invariants::index);
    assert!(inputs >= 500, "only {inputs} index corpus inputs");
    assert!(read >= 100, "only {read} index corpus inputs read as an index");
    assert!(read < inputs, "every index corpus input read, the refused ones too");
}

#[test]
fn every_saved_clipman_input_holds_the_invariants() {
    let (inputs, read) = replay("clipman", invariants::clipman);
    assert!(inputs >= 200, "only {inputs} clipman corpus inputs");
    assert!(read >= 50, "only {read} clipman corpus inputs read");
    assert!(read < inputs, "every clipman corpus input read, the refused ones too");
}
