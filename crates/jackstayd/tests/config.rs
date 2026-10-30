use std::num::NonZeroUsize;

use jackstayd::config::{
    Config, ConfigError, Hotkey, Input, MAX_BYTES, MAX_ENTRIES, MAX_LINK_PORT, MAX_PER_ENTRY,
    MAX_PER_TARGET, MAX_PINNED, MIN_LINK_PORT, parse, render,
};
use proptest::prelude::*;

#[test]
fn an_empty_file_is_the_defaults() {
    let d = Config::default();
    assert_eq!(parse(""), Ok(d.clone()));
    assert_eq!(d.input, Input::default());
    assert_eq!((d.max_entries.get(), d.max_bytes, d.max_pinned), (100, 512 << 20, 1000));
    assert_eq!((d.per_target, d.per_entry), (64 << 20, 128 << 20));
}

#[test]
fn every_key_is_read() {
    let c = parse(
        "[history]\nmax_entries = 7\nmax_bytes = 0\nmax_pinned = 3\n[capture]\nper_target = 10\nper_entry = 10\n",
    );
    let c = c.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        (c.max_entries.get(), c.max_bytes, c.max_pinned, c.per_target, c.per_entry),
        (7, 0, 3, 10, 10)
    );
}

#[test]
fn unknown_keys_and_bad_values_are_refused_by_name() {
    let unknown = |s: &str| ConfigError::UnknownKey(s.to_owned());
    let bad = |s: &str| ConfigError::BadValue(s.to_owned());
    assert_eq!(parse("[history]\nmax_entires = 5\n"), Err(unknown("history.max_entires")));
    assert_eq!(parse("[capture]\nmax_entries = 5\n"), Err(unknown("capture.max_entries")));
    assert_eq!(parse("max_entries = 5\n"), Err(unknown("max_entries")));
    assert_eq!(parse("[history]\nmax_entries = 0\n"), Err(bad("history.max_entries")));
    assert_eq!(parse("[history]\nmax_entries = -1\n"), Err(bad("history.max_entries")));
    assert_eq!(parse("[history]\nmax_entries = \"5\"\n"), Err(bad("history.max_entries")));
    assert_eq!(parse("[history]\nmax_entries = 4294967296\n"), Err(bad("history.max_entries")));
    assert!(parse("[history]\nmax_entries = 4294967295\n").is_ok());
    assert_eq!(parse("[history]\nmax_pinned = 4294967296\n"), Err(bad("history.max_pinned")));
    assert!(parse("[history]\nmax_pinned = 4294967295\n").is_ok());
    assert!(
        parse("[history]\nmax_bytes = 9223372036854775807\n").is_ok(),
        "any TOML integer above 0"
    );
    assert_eq!(parse("[history]\nmax_bytes = -1\n"), Err(bad("history.max_bytes")));
    assert_eq!(parse("[capture]\nper_target = 0\n"), Err(bad("capture.per_target")));
    assert!(parse("[capture]\nper_target = 1073741824\nper_entry = 1073741824\n").is_ok());
    assert_eq!(parse("[capture]\nper_target = 1073741825\n"), Err(bad("capture.per_target")));
    assert!(parse("[capture]\nper_target = 1\nper_entry = 4294967296\n").is_ok());
    assert_eq!(parse("[capture]\nper_entry = 4294967297\n"), Err(bad("capture.per_entry")));
    assert_eq!(parse("[capture]\nper_target = 10\nper_entry = 9\n"), Err(bad("capture.per_entry")));
    assert_eq!(parse("[history"), Err(ConfigError::Syntax));
    assert_eq!(ConfigError::UnknownKey("a.b".into()).to_string(), "unknown configuration key a.b");
    assert_eq!(ConfigError::BadValue("a.b".into()).to_string(), "bad value for a.b");
    assert_eq!(ConfigError::Syntax.to_string(), "the configuration is not valid TOML");
}

#[test]
fn render_writes_only_what_differs_from_the_defaults() {
    let header = "# Written by Jackstay's settings window; a key left out has its default (docs/daemon.md).\n";
    assert_eq!(render(&Config::default()), header);
    let c = Config { max_pinned: 3, per_entry: 1 << 30, ..Config::default() };
    assert_eq!(
        render(&c),
        format!("{header}\n[history]\nmax_pinned = 3\n\n[capture]\nper_entry = 1073741824\n")
    );
    let c = Config { per_target: 5, per_entry: 5, ..Config::default() };
    assert_eq!(render(&c), format!("{header}\n[capture]\nper_target = 5\nper_entry = 5\n"));
}

#[test]
fn input_neighbours_are_read_and_written() {
    let c = parse("[input]\nprimary = true\nleft = \"xiphos\"\nright = \"0123456789abcdef\"\n")
        .unwrap_or_else(|e| panic!("{e}"));
    let want = Input {
        primary: true,
        left: Some("xiphos".into()),
        right: Some("0123456789abcdef".into()),
        ..Input::default()
    };
    assert_eq!(c.input, want);
    let header = "# Written by Jackstay's settings window; a key left out has its default (docs/daemon.md).\n";
    assert_eq!(
        render(&c),
        format!(
            "{header}\n[input]\nprimary = true\nleft = \"xiphos\"\nright = \"0123456789abcdef\"\n"
        )
    );
    let bad = |s: &str| ConfigError::BadValue(s.to_owned());
    assert_eq!(parse("[input]\nleft = \"\"\n"), Err(bad("input.left")));
    assert_eq!(parse("[input]\ntop = 5\n"), Err(bad("input.top")));
    assert_eq!(
        parse(&format!("[input]\nbottom = \"{}\"\n", "x".repeat(256))),
        Err(bad("input.bottom"))
    );
    assert!(parse(&format!("[input]\nbottom = \"{}\"\n", "x".repeat(255))).is_ok());
    assert_eq!(parse("[input]\nprimary = 1\n"), Err(bad("input.primary")));
    assert_eq!(
        parse("[input]\nmiddle = \"x\"\n"),
        Err(ConfigError::UnknownKey("input.middle".into()))
    );
}

#[test]
fn hotkeys_default_to_scroll_lock_and_are_read_and_written() {
    let d = Input::default();
    assert_eq!(d.return_key, Some(Hotkey { sym: 0xff14, shift: false }));
    assert_eq!(d.lock_key, Some(Hotkey { sym: 0xff14, shift: true }));
    let c = parse("[input]\nreturn_key = \"Pause\"\nlock_key = \"none\"\n")
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(c.input.return_key, Some(Hotkey { sym: 0xff13, shift: false }));
    assert_eq!(c.input.lock_key, None);
    let header = "# Written by Jackstay's settings window; a key left out has its default (docs/daemon.md).\n";
    assert_eq!(
        render(&c),
        format!("{header}\n[input]\nreturn_key = \"Pause\"\nlock_key = \"none\"\n")
    );
    for (text, sym, shift) in [
        ("Shift+F13", 0xffca, true),
        ("0xff6b", 0xff6b, false),
        ("Shift+0x1008ff2f", 0x1008_ff2f, true),
    ] {
        let c =
            parse(&format!("[input]\nreturn_key = \"{text}\"\n")).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(c.input.return_key, Some(Hotkey { sym, shift }), "{text}");
        assert_eq!(parse(&render(&c)), Ok(c), "{text}");
    }
    let bad = |s: &str| ConfigError::BadValue(s.to_owned());
    for text in
        ["", "a", "Shift+", "shift+F1", "0x", "0x0", "0x123456789", "0xfg", "F25", "Ctrl+F1"]
    {
        let got = parse(&format!("[input]\nlock_key = \"{text}\"\n"));
        assert_eq!(got, Err(bad("input.lock_key")), "{text}");
    }
    assert_eq!(parse("[input]\nreturn_key = 5\n"), Err(bad("input.return_key")));
}

#[test]
fn a_neighbour_with_quotes_or_control_characters_reads_back() {
    let c = Config {
        input: Input { left: Some("a\"b\\c\u{1b}d\u{7f}é".into()), ..Input::default() },
        ..Config::default()
    };
    assert_eq!(parse(&render(&c)), Ok(c));
}

proptest! {
    #[test]
    fn render_reads_back(
        entries in 1..=MAX_ENTRIES,
        bytes in 0..=MAX_BYTES,
        pinned in 0..=MAX_PINNED,
        target in 1..=MAX_PER_TARGET,
        extra in 0..=MAX_PER_ENTRY,
        link_enabled in any::<bool>(),
        port in MIN_LINK_PORT..=MAX_LINK_PORT,
        primary in any::<bool>(),
        sides in proptest::collection::vec(proptest::option::of("\\PC{1,40}"), 4),
        keys in proptest::collection::vec(proptest::option::of((1u32.., any::<bool>())), 2),
    ) {
        let key = |i: usize| keys.get(i).copied().flatten().map(|(sym, shift)| Hotkey { sym, shift });
        let side = |i: usize| sides.get(i).cloned().flatten();
        let c = Config {
            max_entries: NonZeroUsize::new(usize::try_from(entries).unwrap_or(1)).unwrap_or(NonZeroUsize::MIN),
            max_bytes: bytes,
            max_pinned: usize::try_from(pinned).unwrap_or(0),
            per_target: target,
            per_entry: target.saturating_add(extra).min(MAX_PER_ENTRY),
            link_enabled,
            link_port: u16::try_from(port).unwrap_or(24810),
            input: Input {
                primary,
                left: side(0),
                right: side(1),
                top: side(2),
                bottom: side(3),
                return_key: key(0),
                lock_key: key(1),
            },
        };
        prop_assert_eq!(parse(&render(&c)), Ok(c.clone()));
    }
}
