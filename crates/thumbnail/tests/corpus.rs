//! Replays the fuzzer's saved corpus as an ordinary test (TS-16), so every input that once reached
//! new code is decoded on every run, without nightly.

use std::path::Path;

#[test]
fn every_saved_fuzz_input_decodes_without_panicking() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus/thumbnail");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) => panic!("corpus {} unreadable: {e}", dir.display()),
    };
    let (mut replayed, mut made) = (0usize, 0usize);
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(e) => panic!("corpus entry unreadable: {e}"),
        };
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) => panic!("{} unreadable: {e}", path.display()),
        };
        made += usize::from(jackstay_thumbnail::thumbnail(&bytes).is_ok());
        replayed += 1;
    }
    // A corpus that went missing or empty would make this test pass while replaying nothing (TS-6).
    assert!(replayed >= 50, "only {replayed} corpus inputs in {}", dir.display());
    // And one where the decoder regressed to refusing everything would still replay every file.
    assert!(made >= 5, "only {made} corpus inputs made a thumbnail");
    assert!(made < replayed, "every corpus input made a thumbnail, the malformed ones too");
}
