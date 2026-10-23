#![no_main]

use jackstay_proto::{FRAME_HEADER_LEN, Message, decode, encode};
use libfuzzer_sys::fuzz_target;

// Any input: decoding must not panic; "incomplete" must mean the frame really is incomplete; and a
// decoded frame must use only its own bytes, need all of them, and re-encode to exactly them.
fuzz_target!(|data: &[u8]| match decode(data) {
    Ok(None) => {
        let announced = data.get(..4).map(|h| u32::from_be_bytes([h[0], h[1], h[2], h[3]]));
        let whole = announced.map(|len| FRAME_HEADER_LEN + len as usize);
        assert!(whole.is_none_or(|whole| data.len() < whole), "complete frame reported incomplete");
    }
    Ok(Some((message, used))) => {
        assert!(used <= data.len());
        assert_eq!(decode(&data[..used - 1]), Ok(None));
        let mut again = Vec::new();
        encode(&message, &mut again);
        assert_eq!(again.as_slice(), &data[..used]);
        let Message::Announce(a) = message;
        assert!(!a.targets().is_empty());
    }
    Err(_) => {}
});
