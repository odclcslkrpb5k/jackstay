#![no_main]

use libfuzzer_sys::fuzz_target;

// Any input, as a request and as a response: decoding must not panic; a decoded frame must use
// only its own bytes, need all of them, and re-encode to exactly them.
fuzz_target!(|data: &[u8]| {
    if let Err(e) = jackstay_proto::control::check_decoders(data) {
        panic!("{e}");
    }
});
