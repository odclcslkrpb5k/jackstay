//! The adapter's own duties on a real server: attribution, every owner-change and loss event,
//! cut buffers, the clipboard-manager selection, limits, deadlines, and a lost connection.

use std::time::{Duration, Instant};

use jackstay_clip::{NotKept, TargetData};
use jackstay_selection::{Config, Limits, Report};

use crate::client::{
    Identity, Owner, class_of, cut_buffer, owner_of, properties_of, set_cut_buffer,
};
use crate::harness::{Session, Xvfb, kill, xclip_copy, xclip_paste};

#[test]
fn the_source_is_the_owners_wm_class() {
    let x = Xvfb::start();
    let s = Session::start(&x);
    let id = Identity { wm_class: Some("TestApp".into()), ..Identity::default() };
    let _owner = Owner::start(&x.display, "t", id, true);
    assert_eq!(s.next_capture_with_source().1.as_deref(), Some("TestApp"));
}

#[test]
fn without_wm_class_the_client_leaders_class_names_the_source() {
    let x = Xvfb::start();
    let s = Session::start(&x);
    let id = Identity { leader_class: Some("LeaderApp".into()), ..Identity::default() };
    let _owner = Owner::start(&x.display, "t", id, true);
    assert_eq!(s.next_capture_with_source().1.as_deref(), Some("LeaderApp"));
}

#[test]
fn without_either_the_process_name_names_the_source() {
    let x = Xvfb::start();
    let s = Session::start(&x);
    let id = Identity {
        pid: Some(std::process::id()),
        machine: Some(crate::client::hostname()),
        ..Identity::default()
    };
    let _owner = Owner::start(&x.display, "t", id, true);
    let comm = std::fs::read_to_string("/proc/self/comm").unwrap_or_default();
    assert_eq!(s.next_capture_with_source().1.as_deref(), Some(comm.trim()));
}

#[test]
fn an_owner_destroying_its_window_is_kept_alive() {
    let x = Xvfb::start();
    let s = Session::start(&x);
    let owner = Owner::start(&x.display, "window gone", Identity::default(), true);
    s.next_capture();
    owner.destroy_window();
    assert!(s.wait_for(Duration::from_secs(5), |r| *r == Report::KeptAlive).is_some());
    assert_eq!(xclip_paste(&x, "UTF8_STRING").as_deref(), Some(&b"window gone"[..]));
}

#[test]
fn another_client_taking_the_clipboard_from_us_is_seen() {
    let x = Xvfb::start();
    let s = Session::start(&x);
    s.clear();
    assert!(s.wait_for(Duration::from_secs(5), |r| *r == Report::Claimed).is_some());
    let _owner = Owner::start(&x.display, "mine now", Identity::default(), true);
    assert!(s.wait_for(Duration::from_secs(5), |r| *r == Report::Lost).is_some());
    assert_eq!(
        s.next_capture().kept("UTF8_STRING").map(|k| k.bytes().to_vec()),
        Some(b"mine now".to_vec())
    );
}

/// Waits until the session has started (it then holds `CLIPBOARD_MANAGER`), so an owner started
/// afterwards is seen once, by its notification.
fn ready(x: &Xvfb) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let manager = owner_of(&x.display, "CLIPBOARD_MANAGER");
        if manager != 0 || Instant::now() > deadline {
            return manager;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn we_hold_the_clipboard_manager_selection_on_our_own_window() {
    let x = Xvfb::start();
    let _s = Session::start(&x);
    let manager = ready(&x);
    assert_eq!(class_of(&x.display, manager).as_deref(), Some("Jackstay"));
}

#[test]
fn clear_empties_the_cut_buffers() {
    let x = Xvfb::start();
    let s = Session::start(&x);
    set_cut_buffer(&x.display, b"old secret");
    s.clear();
    assert!(s.wait_for(Duration::from_secs(5), |r| *r == Report::Claimed).is_some());
    assert_eq!(cut_buffer(&x.display), Vec::<u8>::new());
}

fn limited(per_target: usize) -> Config {
    Config { limits: Limits { per_target, per_entry: 1 << 20 } }
}

#[test]
fn a_target_exactly_at_the_limit_is_kept_and_one_byte_over_is_not() {
    for (len, kept) in [(1_000, true), (1_001, false)] {
        let x = Xvfb::start();
        let s = Session::start_with(&x, limited(1_000));
        ready(&x);
        let png = vec![7u8; len];
        let owner = xclip_copy(&x, "image/png", &png);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = None;
        while seen.is_none() && Instant::now() < deadline {
            seen = s.wait_for(Duration::from_millis(500), |r| {
                matches!(r, Report::Captured { complete: true, .. } | Report::CaptureFailed { .. })
            });
        }
        match seen {
            Some(Report::Captured { content, .. }) => {
                assert!(kept, "{len} bytes kept");
                assert_eq!(content.kept("image/png").map(|k| k.bytes().len()), Some(len));
            }
            Some(Report::CaptureFailed { .. }) => assert!(!kept, "{len} bytes refused"),
            other => panic!("{len} bytes: {other:?}"),
        }
        // Nothing is left behind on our window either way.
        let manager = owner_of(&x.display, "CLIPBOARD_MANAGER");
        let left: Vec<String> = properties_of(&x.display, manager)
            .into_iter()
            .filter(|p| p.starts_with("JACKSTAY_P"))
            .collect();
        assert_eq!(left, Vec::<String>::new(), "{len} bytes");
        kill(owner);
        let _ = (NotKept::TooLarge, TargetData::NotKept(NotKept::TooLarge));
    }
}

#[test]
fn an_owner_that_never_answers_gets_the_fallbacks_after_ten_seconds() {
    let x = Xvfb::start();
    let _s = Session::start(&x);
    ready(&x);
    let owner = Owner::start(&x.display, "never", Identity::default(), false);
    let first = owner.requests.recv_timeout(Duration::from_secs(5));
    assert_eq!(first.as_deref(), Ok("TARGETS"));
    let started = Instant::now();
    let second = owner.requests.recv_timeout(Duration::from_secs(15)).unwrap_or_default();
    assert_eq!(second, "UTF8_STRING");
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_secs(9) && waited <= Duration::from_secs(15),
        "after {waited:?}"
    );
}

#[test]
fn a_lost_x_connection_ends_the_session() {
    let x = Xvfb::start();
    let s = Session::start(&x);
    std::thread::sleep(Duration::from_millis(200));
    drop(x);
    let why = s.ended(Duration::from_secs(5)).unwrap_or_default();
    assert!(why.starts_with("X connection lost"), "{why}");
}

#[test]
fn the_session_knows_whether_it_owns_the_clipboard() {
    let x = Xvfb::start();
    let s = Session::start(&x);
    assert!(!s.owns_clipboard());
    s.clear();
    assert!(s.wait_for(Duration::from_secs(5), |r| *r == Report::Claimed).is_some());
    assert!(s.owns_clipboard());
    let _owner = Owner::start(&x.display, "theirs", Identity::default(), true);
    assert!(s.wait_for(Duration::from_secs(5), |r| *r == Report::Lost).is_some());
    assert!(!s.owns_clipboard());
}

#[test]
fn a_requestor_vanishing_mid_incr_ends_its_transfer() {
    let x = Xvfb::start();
    let s = Session::start(&x);
    let big = "x".repeat(600 * 1024);
    let content = {
        let owner = xclip_copy(&x, "UTF8_STRING", big.as_bytes());
        let c = s.next_capture();
        kill(owner);
        c
    };
    s.serve(content);
    assert!(s.wait_for(Duration::from_secs(5), |r| *r == Report::Claimed).is_some());
    assert_eq!(crate::client::request_then_vanish(&x.display), "INCR");
    let deadline = Instant::now() + Duration::from_secs(3);
    while s.timeout().is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(s.timeout(), None, "the transfer to a destroyed window is gone");
}

#[test]
fn the_clipboard_manager_is_announced_to_the_root_window() {
    let x = Xvfb::start();
    let listener = crate::client::RootListener::start(&x.display);
    let _s = Session::start(&x);
    let data = listener.manager_message().unwrap_or_else(|| panic!("no MANAGER message"));
    assert_eq!(data[1], listener.atom("CLIPBOARD_MANAGER"));
    assert_eq!(data[2], owner_of(&x.display, "CLIPBOARD_MANAGER"));
}

#[test]
fn a_pid_from_another_machine_names_nothing() {
    let x = Xvfb::start();
    let s = Session::start(&x);
    let id = Identity {
        pid: Some(std::process::id()),
        machine: Some("elsewhere.example".into()),
        ..Identity::default()
    };
    let _owner = Owner::start(&x.display, "t", id, true);
    assert_eq!(s.next_capture_with_source().1, None);
}

#[test]
fn an_empty_wm_class_instance_still_gives_the_class() {
    let x = Xvfb::start();
    let s = Session::start(&x);
    let id = Identity { wm_class_raw: Some(b"\0Firefox\0".to_vec()), ..Identity::default() };
    let _owner = Owner::start(&x.display, "t", id, true);
    assert_eq!(s.next_capture_with_source().1.as_deref(), Some("Firefox"));
}

#[test]
fn a_stranger_retyping_our_time_property_does_not_wedge_a_claim() {
    let x = Xvfb::start();
    let s = Session::start(&x);
    let ours = owner_of(&x.display, "CLIPBOARD_MANAGER");
    crate::client::put_string(&x.display, ours, "JACKSTAY_TIME", b"not an integer");
    s.clear();
    assert!(s.wait_for(Duration::from_secs(5), |r| *r == Report::Claimed).is_some());
}

#[test]
fn a_stranger_destroying_our_window_ends_the_session_instead_of_hanging_it() {
    let x = Xvfb::start();
    let s = Session::start(&x);
    let ours = owner_of(&x.display, "CLIPBOARD_MANAGER");
    crate::client::destroy(&x.display, ours);
    s.clear();
    assert!(s.ended(Duration::from_secs(5)).is_some(), "the session is still waiting");
}
