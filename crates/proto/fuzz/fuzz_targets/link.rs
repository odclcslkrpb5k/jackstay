#![no_main]

use libfuzzer_sys::fuzz_target;

// Any input, as a link message: decoding must not panic; "incomplete" must mean incomplete; a
// decoded frame must use only its own bytes, need all of them, and re-encode to exactly them.
fuzz_target!(|data: &[u8]| {
    if let Err(e) = jackstay_proto::link::check_decoder(data) {
        panic!("{e}");
    }
});
