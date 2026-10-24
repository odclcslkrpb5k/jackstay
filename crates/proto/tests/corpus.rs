//! Replays the fuzzer's saved corpus as an ordinary test (TS-16), so every input that once reached
//! new code is decoded on every run, without nightly.

use std::path::Path;

use jackstay_proto::{FRAME_HEADER_LEN, decode, encode};

#[test]
fn every_saved_fuzz_input_decodes_without_panicking() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus/decode");
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
        match decode(&bytes) {
            Ok(Some((message, used))) => {
                let mut again = Vec::new();
                encode(&message, &mut again);
                assert_eq!(Some(again.as_slice()), bytes.get(..used), "{}", path.display());
                decoded += 1;
            }
            Ok(None) => {
                let announced = bytes.get(..4).and_then(|h| <[u8; 4]>::try_from(h).ok());
                let whole = announced.map(|h| FRAME_HEADER_LEN + u32::from_be_bytes(h) as usize);
                assert!(whole.is_none_or(|w| bytes.len() < w), "{} is complete", path.display());
            }
            Err(_) => {}
        }
        replayed += 1;
    }
    // A corpus that went missing or empty would make this test pass while replaying nothing (TS-6).
    assert!(replayed >= 20, "only {replayed} corpus inputs in {}", dir.display());
    // And one where the decoder regressed to refusing everything would still replay every file.
    assert!(decoded >= 3, "only {decoded} corpus inputs decoded to a message");
}

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
