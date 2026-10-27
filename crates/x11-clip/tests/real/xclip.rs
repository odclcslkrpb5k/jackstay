//! Copy and paste through xclip and xsel.

use std::time::Duration;

use jackstay_selection::Report;

use crate::harness::{
    Session, Xvfb, kill, wait_unowned, xclip_copy, xclip_copy_to, xclip_paste, xclip_paste_from,
    xsel_paste,
};

fn kept(content: &jackstay_clip::Content, target: &str) -> Option<Vec<u8>> {
    content.kept(target).map(|k| k.bytes().to_vec())
}

#[test]
fn a_copy_is_captured_and_survives_its_owner_exiting() {
    let x = Xvfb::start();
    let s = Session::start(&x);
    let owner = xclip_copy(&x, "UTF8_STRING", "héllo".as_bytes());
    let content = s.next_capture();
    assert_eq!(kept(&content, "UTF8_STRING").as_deref(), Some("héllo".as_bytes()));
    kill(owner);
    assert!(s.wait_for(Duration::from_secs(5), |r| *r == Report::KeptAlive).is_some());
    assert_eq!(xclip_paste(&x, "UTF8_STRING").as_deref(), Some("héllo".as_bytes()));
    assert_eq!(
        xsel_paste(&x).as_deref(),
        Some("héllo".as_bytes()),
        "xsel asks for its own targets"
    );
}

/// Beyond the server's 16 MiB request limit (with BIG-REQUESTS), so xclip itself must send it by
/// INCR, and we must serve it back the same way.
#[test]
fn twenty_megabytes_go_by_incr_both_ways() {
    let x = Xvfb::start();
    let s = Session::start(&x);
    let big: Vec<u8> = (0..20u32 << 20).map(|i| b'a' + u8::try_from(i % 26).unwrap_or(0)).collect();
    let owner = xclip_copy(&x, "UTF8_STRING", &big);
    let content = s.next_capture();
    assert_eq!(kept(&content, "UTF8_STRING").map(|k| k.len()), Some(big.len()));
    kill(owner);
    assert!(s.wait_for(Duration::from_secs(5), |r| *r == Report::KeptAlive).is_some());
    assert!(xclip_paste(&x, "UTF8_STRING") == Some(big), "the pasted 20 MiB differ");
}

#[test]
fn an_image_comes_back_byte_for_byte() {
    let x = Xvfb::start();
    let s = Session::start(&x);
    let png: Vec<u8> = b"\x89PNG\r\n\x1a\n"
        .iter()
        .copied()
        .chain((0..300_000u32).map(|i| (i % 251) as u8))
        .collect();
    let owner = xclip_copy(&x, "image/png", &png);
    let content = s.next_capture();
    assert_eq!(kept(&content, "image/png").map(|k| k.len()), Some(png.len()));
    kill(owner);
    assert!(s.wait_for(Duration::from_secs(5), |r| *r == Report::KeptAlive).is_some());
    assert!(xclip_paste(&x, "image/png") == Some(png), "the pasted image differs");
}

#[test]
fn clear_leaves_nothing_to_paste() {
    let x = Xvfb::start();
    let s = Session::start(&x);
    let owner = xclip_copy(&x, "UTF8_STRING", b"secret");
    s.next_capture();
    s.clear();
    assert!(s.wait_for(Duration::from_secs(5), |r| *r == Report::Claimed).is_some());
    let pasted = xclip_paste(&x, "UTF8_STRING");
    assert!(pasted.is_none() || pasted.as_deref() == Some(&b""[..]), "pasted {pasted:?}");
    kill(owner);
}

#[test]
fn clear_empties_primary_too() {
    let x = Xvfb::start();
    let s = Session::start(&x);
    let selected = xclip_copy_to(&x, "primary", "UTF8_STRING", b"selected secret");
    // PRIMARY is not captured, so no report says when xclip has it: ask until it answers.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while xclip_paste_from(&x, "primary", "UTF8_STRING").as_deref() != Some(&b"selected secret"[..])
    {
        assert!(std::time::Instant::now() < deadline, "xclip never owned PRIMARY");
        std::thread::sleep(Duration::from_millis(20));
    }
    s.clear();
    assert!(s.wait_for(Duration::from_secs(5), |r| *r == Report::Claimed).is_some());
    let pasted = xclip_paste_from(&x, "primary", "UTF8_STRING");
    assert!(pasted.is_none() || pasted.as_deref() == Some(&b""[..]), "pasted {pasted:?}");
    kill(selected);
}

#[test]
fn restore_fills_only_an_empty_clipboard() {
    let x = Xvfb::start();
    let s = Session::start(&x);
    let owner = xclip_copy(&x, "UTF8_STRING", b"live");
    let saved = s.next_capture();
    let newer = xclip_copy(&x, "UTF8_STRING", b"newer");
    s.next_capture();
    s.restore(saved.clone());
    assert!(s.wait_for(Duration::from_secs(5), |r| *r == Report::ClaimFailed).is_some());
    assert_eq!(xclip_paste(&x, "UTF8_STRING").as_deref(), Some(&b"newer"[..]));
    kill(owner);
    kill(newer);
    drop(s);
    wait_unowned(&x);

    // A fresh session on a clipboard whose owner has gone.
    let s = Session::start(&x);
    s.restore(saved);
    assert!(s.wait_for(Duration::from_secs(5), |r| *r == Report::Claimed).is_some());
    assert_eq!(xclip_paste(&x, "UTF8_STRING").as_deref(), Some(&b"live"[..]));
}

#[test]
fn debug_output_of_the_clipboard_holds_no_contents() {
    let x = Xvfb::start();
    let s = Session::start(&x);
    let owner = xclip_copy(&x, "UTF8_STRING", b"hunter2-captured");
    s.next_capture();
    let printed = s.debug();
    assert!(!printed.contains("hunter2") && !printed.contains("104, 117, 110"), "{printed}");
    // Between turns x11rb's buffers are empty; within one they hold requests and replies carrying
    // contents, so the connection is never printed at all.
    assert!(!printed.contains("pending_replies") && !printed.contains("data_buf"), "{printed}");
    assert!(printed.contains("port: X11Port { root:"), "{printed}");
    kill(owner);
    assert!(s.wait_for(Duration::from_secs(5), |r| *r == Report::KeptAlive).is_some());
    assert_eq!(xclip_paste(&x, "UTF8_STRING").as_deref(), Some(&b"hunter2-captured"[..]));
    let printed = s.debug();
    assert!(!printed.contains("hunter2") && !printed.contains("104, 117, 110"), "{printed}");
}

#[test]
fn serve_puts_an_entry_back_on_the_clipboard() {
    let x = Xvfb::start();
    let s = Session::start(&x);
    let owner = xclip_copy(&x, "UTF8_STRING", b"first");
    let first = s.next_capture();
    let second = xclip_copy(&x, "UTF8_STRING", b"second");
    s.next_capture();
    s.serve(first);
    assert!(s.wait_for(Duration::from_secs(5), |r| *r == Report::Claimed).is_some());
    assert_eq!(xclip_paste(&x, "UTF8_STRING").as_deref(), Some(&b"first"[..]));
    kill(owner);
    kill(second);
}
