#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Err(e) = jackstay_store::invariants::clipman(data) {
        panic!("{e}");
    }
});
