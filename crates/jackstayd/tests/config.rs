use std::num::NonZeroUsize;

use jackstayd::config::{
    Config, ConfigError, MAX_BYTES, MAX_ENTRIES, MAX_PER_ENTRY, MAX_PER_TARGET, MAX_PINNED, parse,
    render,
};
use proptest::prelude::*;

#[test]
fn an_empty_file_is_the_defaults() {
    let d = Config::default();
    assert_eq!(parse(""), Ok(d));
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

proptest! {
    #[test]
    fn render_reads_back(
        entries in 1..=MAX_ENTRIES,
        bytes in 0..=MAX_BYTES,
        pinned in 0..=MAX_PINNED,
        target in 1..=MAX_PER_TARGET,
        extra in 0..=MAX_PER_ENTRY,
    ) {
        let c = Config {
            max_entries: NonZeroUsize::new(usize::try_from(entries).unwrap_or(1)).unwrap_or(NonZeroUsize::MIN),
            max_bytes: bytes,
            max_pinned: usize::try_from(pinned).unwrap_or(0),
            per_target: target,
            per_entry: target.saturating_add(extra).min(MAX_PER_ENTRY),
        };
        prop_assert_eq!(parse(&render(&c)), Ok(c));
    }
}
