//! Replays the fuzzer's saved corpus as an ordinary test (TS-16), so every input that once reached
//! new code is decoded on every run, without nightly.

use std::path::Path;

#[test]
fn every_saved_control_input_holds_the_decoders_properties() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus/control");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) => panic!("corpus {} unreadable: {e}", dir.display()),
    };
    let (mut replayed, mut decoded) = (0usize, 0usize);
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(e) => panic!("corpus entry unreadable: {e}"),
        };
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) => panic!("{} unreadable: {e}", path.display()),
        };
        if let Err(e) = jackstay_proto::control::check_decoders(&bytes) {
            panic!("{}: {e}", path.display());
        }
        let request = jackstay_proto::control::decode_request(&bytes);
        let response = jackstay_proto::control::decode_response(&bytes);
        decoded += usize::from(matches!(request, Ok(Some(_))) || matches!(response, Ok(Some(_))));
        replayed += 1;
    }
    assert!(replayed >= MIN_CONTROL_INPUTS, "only {replayed} control corpus inputs");
    assert!(decoded >= 10, "only {decoded} control corpus inputs decoded");
    assert!(decoded < replayed, "every control corpus input decoded, the malformed ones too");
}

const MIN_CONTROL_INPUTS: usize = 100;

#[test]
fn every_saved_link_input_holds_the_decoders_properties() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus/link");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) => panic!("corpus {} unreadable: {e}", dir.display()),
    };
    let (mut replayed, mut decoded) = (0usize, 0usize);
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(e) => panic!("corpus entry unreadable: {e}"),
        };
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) => panic!("{} unreadable: {e}", path.display()),
        };
        if let Err(e) = jackstay_proto::link::check_decoder(&bytes) {
            panic!("{}: {e}", path.display());
        }
        decoded += usize::from(matches!(jackstay_proto::link::decode(&bytes), Ok(Some(_))));
        replayed += 1;
    }
    assert!(replayed >= MIN_LINK_INPUTS, "only {replayed} link corpus inputs");
    assert!(decoded >= 7, "only {decoded} link corpus inputs decoded");
    assert!(decoded < replayed, "every link corpus input decoded, the malformed ones too");
}

const MIN_LINK_INPUTS: usize = 15;
