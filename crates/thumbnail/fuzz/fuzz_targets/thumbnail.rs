#![no_main]

use libfuzzer_sys::fuzz_target;

// Any input, as a clipboard image: decoding must not panic, and a thumbnail made fits its limits
// (which `Thumbnail::new` holds to).
fuzz_target!(|data: &[u8]| {
    let _ = jackstay_thumbnail::thumbnail(data);
});
