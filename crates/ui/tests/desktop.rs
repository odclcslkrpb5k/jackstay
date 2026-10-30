//! The settings window's parts that need no display: Xfce's shortcut listing, the autostart
//! entry, the fields' mapping to the configuration, and writing a file whole.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use jackstay_ui::desktop::{
    Places, bound_to, listing, load, starts_at_login, taken, with_hidden, write_file,
};
use jackstay_ui::form::{self, Fields, RANGES};
use jackstay_ui::settings::hotkey_modifiers;
use jackstayd::config::{
    Config, MAX_BYTES, MAX_ENTRIES, MAX_PER_ENTRY, MAX_PER_TARGET, MAX_PINNED,
};

#[track_caller]
fn ok<T, E: std::fmt::Debug>(r: Result<T, E>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("{e:?}"),
    }
}

fn dir(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("desktop-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

const POPUP: &str = "/home/u/.local/bin/jackstay popup";

/// As `xfconf-query -c xfce4-keyboard-shortcuts -l -v` prints it: names padded to a column.
const LISTING: &str = "\
/commands/custom/<Alt>F2               xfce4-appfinder --collapsed
/commands/custom/<Primary><Alt>h       /home/u/.local/bin/jackstay popup
/commands/custom/override              true
/commands/default/<Super>e             thunar
/commands/default/<Primary><Alt>v      /home/u/.local/bin/jackstay popup
/xfwm4/custom/<Alt>Tab                 cycle_windows_key
/xfwm4/custom/<Super>d                 show_desktop_key
/commands/custom/<Super>v              /home/u/.local/bin/jackstay popup
";

fn same(a: &str, b: &str) -> bool {
    a == b
}

#[test]
fn the_shortcut_listing_is_read_and_the_popups_keys_found() {
    let s = listing(LISTING);
    assert_eq!(s.len(), 8);
    assert_eq!(
        s[0],
        ("/commands/custom/<Alt>F2".to_owned(), "xfce4-appfinder --collapsed".to_owned())
    );
    // Only bindings of its own commands, not the defaults' copy.
    assert_eq!(bound_to(&s, POPUP), ["<Primary><Alt>h", "<Super>v"]);
    assert_eq!(bound_to(&s, "/elsewhere/jackstay popup"), Vec::<String>::new());
    assert_eq!(listing("no-value-line\n\n"), []);
}

#[test]
fn keys_taken_by_a_command_or_the_window_manager_are_named() {
    let s = listing(LISTING);
    assert_eq!(taken(&s, "<Alt>F2", POPUP, same), Some("runs xfce4-appfinder --collapsed".into()));
    assert_eq!(
        taken(&s, "<Alt>Tab", POPUP, same),
        Some("is the window manager's cycle_windows_key".into())
    );
    assert_eq!(taken(&s, "<Primary><Alt>h", POPUP, same), None, "already the popup's");
    assert_eq!(taken(&s, "<Super>e", POPUP, same), None, "a default's copy");
    assert_eq!(taken(&s, "<Primary><Alt>j", POPUP, same), None);
    // Until a group is overridden, Xfce copies its defaults into its custom bindings.
    let fresh =
        listing("/commands/default/<Super>e thunar\n/xfwm4/default/<Alt>F4 close_window_key\n");
    assert_eq!(taken(&fresh, "<Super>e", POPUP, same), Some("runs thunar".into()));
    assert_eq!(
        taken(&fresh, "<Alt>F4", POPUP, same),
        Some("is the window manager's close_window_key".into())
    );
    let wm_only = listing(
        "/xfwm4/custom/override true\n/xfwm4/default/<Alt>F4 close_window_key\n\
         /commands/default/<Super>e thunar\n",
    );
    assert_eq!(taken(&wm_only, "<Alt>F4", POPUP, same), None, "overridden: defaults not in force");
    assert!(taken(&wm_only, "<Super>e", POPUP, same).is_some(), "each group on its own");
    assert_eq!(taken(&s, "override", POPUP, same), None, "the flag is not a binding");
    // As the window compares them: by what the names mean.
    let loose = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
    assert!(taken(&s, "<alt>f2", POPUP, loose).is_some());
}

#[test]
fn starting_at_login_is_the_entrys_hidden_key() {
    let entry = "[Desktop Entry]\nType=Application\nExec=/b/jackstay-session\n";
    assert!(starts_at_login(entry));
    assert!(!starts_at_login(&with_hidden(entry, true)));
    assert_eq!(
        with_hidden(entry, true),
        "[Desktop Entry]\nHidden=true\nType=Application\nExec=/b/jackstay-session\n"
    );
    assert!(starts_at_login(&with_hidden(&with_hidden(entry, true), false)));
    // Replaced where it is, spaces around `=` as GKeyFile allows; another group's is not it.
    let spaced = "[Desktop Entry]\nName=J\nHidden = true\n[Desktop Action x]\nHidden=true\n";
    assert!(!starts_at_login(spaced));
    assert_eq!(
        with_hidden(spaced, false),
        "[Desktop Entry]\nHidden=false\nName=J\n[Desktop Action x]\nHidden=true\n"
    );
    assert!(starts_at_login("[Desktop Action x]\nHidden=true\n[Desktop Entry]\nName=J\n"));
    assert!(starts_at_login("[Desktop Entry]\nHidden=false\n"));
    assert!(starts_at_login("[Desktop Entry]\nX-Hidden=true\n"));
    // An entry without its group gets one.
    assert_eq!(with_hidden("", true), "[Desktop Entry]\nHidden=true\n");
}

#[test]
fn a_file_is_written_whole_with_its_mode_and_directory() {
    let d = dir("write");
    let path = d.join("jackstay").join("config.toml");
    ok(write_file(&path, "one\n", 0o600));
    assert_eq!(ok(std::fs::read_to_string(&path)), "one\n");
    let mode = ok(std::fs::metadata(&path)).permissions();
    assert_eq!(std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777, 0o600);
    // Over the old one, and over a temporary file a crash left.
    let stale = d.join("jackstay").join(format!("config.toml.tmp-{}", std::process::id()));
    ok(std::fs::write(&stale, "stale"));
    ok(write_file(&path, "two\n", 0o600));
    assert_eq!(ok(std::fs::read_to_string(&path)), "two\n");
    assert_eq!(ok(std::fs::read_dir(path.parent().unwrap_or(&d))).count(), 1, "nothing left over");
    // Through a symbolic link, which stays.
    let real = d.join("dotfiles").join("config.toml");
    ok(std::fs::create_dir_all(real.parent().unwrap_or(&d)));
    ok(std::fs::write(&real, "old\n"));
    let link = d.join("linked").join("config.toml");
    ok(std::fs::create_dir_all(link.parent().unwrap_or(&d)));
    ok(std::os::unix::fs::symlink(&real, &link));
    ok(write_file(&link, "new\n", 0o600));
    assert!(link.is_symlink());
    assert_eq!(ok(std::fs::read_to_string(&real)), "new\n");
    // A failure leaves the file as it was, and nothing beside it.
    let blocked = d.join("blocked");
    ok(std::fs::create_dir_all(blocked.join("config.toml")));
    assert!(write_file(&blocked.join("config.toml"), "x", 0o600).is_err());
    assert_eq!(ok(std::fs::read_dir(&blocked)).count(), 1);
}

#[test]
fn a_configuration_missing_is_the_defaults_and_one_unreadable_says_so() {
    let d = dir("load");
    let places = Places {
        config: d.join("config.toml"),
        autostart: d.join("jackstay.desktop"),
        popup_command: POPUP.to_owned(),
    };
    let loaded = load(&places);
    assert_eq!((loaded.config, loaded.config_problem), (Config::default(), None));
    assert_eq!(loaded.login, None, "no autostart entry");
    ok(std::fs::create_dir_all(d.join("config.toml")));
    ok(std::fs::write(d.join("jackstay.desktop"), "[Desktop Entry]\nHidden=true\n"));
    let loaded = load(&places);
    assert_eq!(loaded.config, Config::default());
    assert_eq!(loaded.config_problem.as_deref(), Some("unreadable: is a directory"));
    assert_eq!(loaded.login, Some(false));
    let _ = std::fs::remove_dir(d.join("config.toml"));
    ok(std::fs::write(d.join("config.toml"), "[history]\nmax_pinned = 2\n"));
    assert_eq!(load(&places).config.max_pinned, 2);
}

#[test]
fn places_follow_the_xdg_directories() {
    let env = |vars: &'static [(&'static str, &'static str)]| {
        move |n: &str| vars.iter().find(|(k, _)| *k == n).map(|(_, v)| std::ffi::OsString::from(*v))
    };
    let bin = Path::new("/home/u/.local/bin");
    let p = Places::from_env(&env(&[("HOME", "/home/u")]), bin);
    let p = p.unwrap_or_else(|| panic!("no places"));
    assert_eq!(p.config, Path::new("/home/u/.config/jackstay/config.toml"));
    assert_eq!(p.autostart, Path::new("/home/u/.config/autostart/jackstay.desktop"));
    assert_eq!(p.popup_command, POPUP, "as tools/install binds it");
    let p = Places::from_env(&env(&[("HOME", "/home/u"), ("XDG_CONFIG_HOME", "/c")]), bin);
    assert_eq!(p.map(|p| p.config), Some("/c/jackstay/config.toml".into()));
    let p = Places::from_env(&env(&[("HOME", "/home/u"), ("XDG_CONFIG_HOME", "rel")]), bin);
    assert_eq!(p.map(|p| p.config), Some("/home/u/.config/jackstay/config.toml".into()));
    assert!(Places::from_env(&env(&[("HOME", "rel")]), bin).is_none());
    assert!(Places::from_env(&env(&[]), bin).is_none());
}

#[test]
fn fields_show_sizes_in_mib_and_keep_exact_bytes_unless_changed() {
    let d = Config::default();
    let f = form::fields(&d);
    assert_eq!(
        f,
        Fields { entries: 100, size_mib: 512, pinned: 1000, target_mib: 64, entry_mib: 128 }
    );
    assert_eq!(form::config(&f, &d), Ok(d.clone()));
    let odd = Config { max_bytes: 1000, per_target: 3 << 19, per_entry: 5 << 20, ..d.clone() };
    let f = form::fields(&odd);
    assert_eq!((f.size_mib, f.target_mib, f.entry_mib), (0, 2, 5), "to the nearest MiB");
    assert_eq!(form::config(&f, &odd), Ok(odd.clone()), "unchanged fields keep their bytes");
    let changed = Fields { entries: 7, size_mib: 1, target_mib: 3, ..f };
    let want = Config {
        max_entries: NonZeroUsize::new(7).unwrap_or(NonZeroUsize::MIN),
        max_bytes: 1 << 20,
        per_target: 3 << 20,
        ..odd.clone()
    };
    assert_eq!(form::config(&changed, &odd), Ok(want));
    // Within the field's range at the top too, and kept exact.
    let top = Config { max_bytes: MAX_BYTES, ..d.clone() };
    assert_eq!(form::fields(&top).size_mib, RANGES.size_mib.1);
    assert_eq!(form::config(&form::fields(&top), &top), Ok(top.clone()));
    // At least the lowest a field allows, even for a size of a few bytes.
    let tiny = Config { per_target: 1, per_entry: 1, ..d };
    assert_eq!(form::fields(&tiny).target_mib, 1);
    assert_eq!(form::config(&form::fields(&tiny), &tiny), Ok(tiny.clone()));
    // Settings the window does not show are kept as the file has them.
    let input = jackstayd::config::Input {
        primary: true,
        left: Some("xiphos".into()),
        right: Some("xiphos".into()),
        ..jackstayd::config::Input::default()
    };
    let linked = Config { link_port: 30000, input, ..d };
    let changed = Fields { entries: 9, ..form::fields(&linked) };
    let saved = form::config(&changed, &linked).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!((saved.link_port, saved.input), (30000, linked.input));
}

#[test]
fn fields_the_daemon_would_refuse_are_refused_by_name() {
    let d = Config::default();
    let f = form::fields(&d);
    assert_eq!(
        form::config(&Fields { target_mib: 200, ..f }, &d),
        Err("Most kept from one copy must be at least the largest item kept.".into())
    );
    assert_eq!(
        form::config(&Fields { entries: 0, ..f }, &d),
        Err("Entries kept is out of range.".into())
    );
    assert_eq!(
        form::config(&Fields { entries: MAX_ENTRIES + 1, ..f }, &d),
        Err("Entries kept is out of range.".into())
    );
    assert_eq!(
        form::config(&Fields { pinned: MAX_PINNED + 1, ..f }, &d),
        Err("Pinned entries is out of range.".into())
    );
    assert_eq!(
        form::config(&Fields { target_mib: 1025, entry_mib: 2000, ..f }, &d),
        Err("Largest item kept is out of range.".into())
    );
    assert_eq!(
        form::config(&Fields { entry_mib: 4097, ..f }, &d),
        Err("Most kept from one copy is out of range.".into()),
        "above the most allowed"
    );
    assert_eq!(
        form::config(&Fields { size_mib: (MAX_BYTES >> 20) + 1, ..f }, &d),
        Err("Size kept is out of range.".into())
    );
}

#[test]
fn the_fields_ranges_are_the_daemons() {
    assert_eq!(RANGES.entries, (1, MAX_ENTRIES));
    assert_eq!(RANGES.size_mib, (0, MAX_BYTES >> 20));
    assert_eq!(RANGES.pinned, (0, MAX_PINNED));
    assert_eq!(RANGES.target_mib, (1, MAX_PER_TARGET >> 20));
    assert_eq!(RANGES.entry_mib, (1, MAX_PER_ENTRY >> 20));
    let top = Fields {
        entries: RANGES.entries.1,
        size_mib: RANGES.size_mib.1,
        pinned: RANGES.pinned.1,
        target_mib: RANGES.target_mib.1,
        entry_mib: RANGES.entry_mib.1,
    };
    assert!(form::config(&top, &Config::default()).is_ok(), "the top of every range is saved");
}

#[test]
fn a_hotkey_needs_ctrl_alt_or_super_unless_it_is_a_function_key() {
    use gtk::gdk::ModifierType as M;
    use gtk::gdk::keys::constants as k;
    let none = M::empty();
    assert_eq!(hotkey_modifiers(*k::j, none), None, "a key people type");
    assert_eq!(hotkey_modifiers(*k::j, M::SHIFT_MASK), None);
    assert_eq!(hotkey_modifiers(*k::j, M::CONTROL_MASK), Some(M::CONTROL_MASK));
    assert_eq!(hotkey_modifiers(*k::j, M::MOD1_MASK), Some(M::MOD1_MASK));
    assert_eq!(hotkey_modifiers(*k::F1, none), Some(none));
    assert_eq!(hotkey_modifiers(*k::F35, M::SHIFT_MASK), Some(M::SHIFT_MASK));
    // Super as X reports it, and what is not a modifier of a hotkey (Num Lock, a button).
    assert_eq!(hotkey_modifiers(*k::v, M::MOD4_MASK), Some(M::SUPER_MASK));
    let noise = M::MOD2_MASK | M::BUTTON1_MASK | M::LOCK_MASK;
    assert_eq!(hotkey_modifiers(*k::v, M::CONTROL_MASK | noise), Some(M::CONTROL_MASK));
    assert_eq!(hotkey_modifiers(*k::v, noise), None);
    // Every modifier Xfce names is kept in the name.
    let all = M::CONTROL_MASK
        | M::SHIFT_MASK
        | M::MOD1_MASK
        | M::SUPER_MASK
        | M::HYPER_MASK
        | M::META_MASK;
    assert_eq!(hotkey_modifiers(*k::v, all | noise), Some(all));
}
