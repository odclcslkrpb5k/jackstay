//! The settings window, run for real on Xvfb and driven with xdotool, with `xfconf-query` and
//! `systemctl` stood in for by scripts that record what they are asked.

mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use jackstayd::ui_socket::{self, Ask};
use support::{Ui, eventually, ok, start_with};

const HEADER: &str =
    "# Written by Jackstay's settings window; a key left out has its default (docs/daemon.md).\n";
const ENTRY: &str = "[Desktop Entry]\nType=Application\nName=Jackstay\nExec=/b/jackstay-session\n";

/// What the window's commands and files are, for one UI.
struct Desk {
    ui: Ui,
    root: PathBuf,
}

/// The command a hotkey runs: `jackstay` beside the UI, as `tools/install` puts it.
fn popup_command() -> String {
    let bin = Path::new(env!("CARGO_BIN_EXE_jackstay-ui")).parent().unwrap_or(Path::new("/"));
    format!("{} popup", bin.join("jackstay").display())
}

fn script(path: &Path, body: &str) {
    ok(std::fs::write(path, format!("#!/bin/bash\n{body}")));
    ok(std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755)));
}

/// A UI whose configuration lives under a directory of its own; `shortcuts` are Xfce's to begin
/// with ("property value", one a line), and the autostart entry is `entry` if any.
fn desk(name: &str, shortcuts: &str, entry: Option<&str>) -> Desk {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("settings-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let fakes = root.join("fakes");
    ok(std::fs::create_dir_all(&fakes));
    ok(std::fs::create_dir_all(root.join("config/autostart")));
    ok(std::fs::write(root.join("xfconf"), shortcuts));
    if let Some(entry) = entry {
        ok(std::fs::write(root.join("config/autostart/jackstay.desktop"), entry));
    }
    // As tools/test-install's: properties one a line, "path value".
    script(
        &fakes.join("xfconf-query"),
        &format!(
            "store='{store}'\n[[ -e '{broken}' ]] && {{ echo 'Channel unknown' >&2; exit 1; }}\n\
             printf '%s\\n' \"$*\" >>'{calls}'\n{body}",
            store = root.join("xfconf").display(),
            broken = root.join("xfconf-broken").display(),
            calls = root.join("xfconf-calls").display(),
            body = r#"p="" v="" list=0 remove=0
while (($#)); do
  case "$1" in
    -c | -t) shift ;;
    -p) p=$2; shift ;;
    -s) v=$2; shift ;;
    -l) list=1 ;;
    -r) remove=1 ;;
  esac
  shift
done
if ((list)); then grep -F "$p/" "$store" || true
elif ((remove)); then grep -vF "$p " "$store" >"$store.new" || true; mv "$store.new" "$store"
else printf '%s %s\n' "$p" "$v" >>"$store"; fi
"#
        ),
    );
    script(
        &fakes.join("systemctl"),
        &format!(
            "printf '%s\\n' \"$*\" >>'{calls}'\n[[ -e '{slow}' ]] && sleep 1\n[[ -e '{fails}' ]] && {{ echo 'Failed to connect to bus' >&2; exit 1; }}\nexit 0\n",
            calls = root.join("systemctl-calls").display(),
            fails = root.join("systemctl-fails").display(),
            slow = root.join("systemctl-slow").display(),
        ),
    );
    let path = format!("{}:{}", fakes.display(), std::env::var("PATH").unwrap_or_default());
    let config_home = root.join("config");
    let ui = start_with(name, |c| {
        c.env("PATH", &path).env("XDG_CONFIG_HOME", &config_home);
    });
    Desk { ui, root }
}

impl Desk {
    fn config(&self) -> PathBuf {
        self.root.join("config/jackstay/config.toml")
    }

    fn entry(&self) -> String {
        ok(std::fs::read_to_string(self.root.join("config/autostart/jackstay.desktop")))
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.root.join(name)).unwrap_or_default()
    }

    fn visible(&self) -> bool {
        let out = ok(self
            .ui
            .x
            .command("xdotool")
            .args(["search", "--onlyvisible", "--name", "Jackstay settings"])
            .output());
        out.status.success()
    }

    /// `jackstay settings`, and the window shown with the keyboard.
    fn open(&self) {
        ok(ui_socket::request(&self.ui.runtime, Ask::Settings, Duration::from_secs(5)));
        eventually("the settings window", || self.visible());
        // Settled: shown, filled and focused before keys are sent.
        std::thread::sleep(Duration::from_millis(300));
    }

    fn keys(&self, keys: &[&str]) {
        self.ui.keys(keys);
    }

    /// A button's mnemonic: GTK 3 shows the press for 250 ms before the click, and keys sent
    /// sooner would arrive first.
    fn press(&self, mnemonic: &str) {
        self.ui.keys(&[mnemonic]);
        std::thread::sleep(Duration::from_millis(400));
    }

    fn typed(&self, text: &str) {
        self.ui.x.xdotool(&["type", "--delay", "40", text]);
    }

    fn closed(&self) {
        eventually("the settings window to close", || !self.visible());
    }

    /// Still open a moment after, as after a refused save.
    fn stays_open(&self) {
        std::thread::sleep(Duration::from_millis(500));
        assert!(self.visible(), "the window closed");
    }
}

#[test]
fn saving_a_limit_writes_only_what_changed_and_restarts_the_daemon() {
    let d = desk("limits", "", Some(ENTRY));
    // A size given in bytes, not whole MiB.
    ok(std::fs::create_dir_all(d.root.join("config/jackstay")));
    ok(std::fs::write(d.config(), "[history]\nmax_bytes = 1000\n"));
    d.open();
    // Nothing changed: closed, nothing written or restarted.
    d.keys(&["alt+s"]);
    d.closed();
    assert_eq!(ok(std::fs::read_to_string(d.config())), "[history]\nmax_bytes = 1000\n");
    assert_eq!(d.read("systemctl-calls"), "");
    // Entries kept, typed over.
    d.open();
    d.keys(&["alt+e", "ctrl+a"]);
    d.typed("20");
    d.keys(&["alt+s"]);
    d.closed();
    assert_eq!(
        ok(std::fs::read_to_string(d.config())),
        format!("{HEADER}\n[history]\nmax_entries = 20\nmax_bytes = 1000\n"),
        "the exact bytes kept"
    );
    let mode = ok(std::fs::metadata(d.config())).permissions();
    assert_eq!(std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777, 0o600);
    eventually("the restart", || {
        d.read("systemctl-calls") == "--user --no-block restart jackstayd.service\n"
    });
    assert_eq!(d.entry(), ENTRY, "the autostart entry untouched");
    assert!(!d.read("xfconf-calls").contains(" -s "), "no hotkey set");
    // Cancel and Esc forget what was typed.
    for close in ["alt+c", "Escape"] {
        d.open();
        d.keys(&["alt+e", "ctrl+a"]);
        d.typed("33");
        d.keys(&[close]);
        d.closed();
    }
    assert!(ok(std::fs::read_to_string(d.config())).contains("max_entries = 20\n"));
}

#[test]
fn a_limit_the_daemon_would_refuse_is_not_saved() {
    let d = desk("refused", "", Some(ENTRY));
    d.open();
    // Largest item kept above the most kept from one copy (128 MiB).
    d.keys(&["alt+i", "ctrl+a"]);
    d.typed("200");
    d.keys(&["alt+s"]);
    d.stays_open();
    assert!(!d.config().exists());
    assert_eq!(d.read("systemctl-calls"), "");
    // Corrected, it saves.
    d.keys(&["alt+m", "ctrl+a"]);
    d.typed("300");
    d.keys(&["alt+s"]);
    d.closed();
    assert_eq!(
        ok(std::fs::read_to_string(d.config())),
        format!("{HEADER}\n[capture]\nper_target = 209715200\nper_entry = 314572800\n")
    );
}

#[test]
fn a_configuration_the_daemon_refuses_is_replaced_on_save() {
    let d = desk("invalid", "", Some(ENTRY));
    ok(std::fs::create_dir_all(d.root.join("config/jackstay")));
    ok(std::fs::write(d.config(), "[history]\nmax_entires = 5\n"));
    d.open();
    d.keys(&["alt+s"]);
    d.closed();
    assert_eq!(ok(std::fs::read_to_string(d.config())), HEADER, "the defaults");
    eventually("the restart", || {
        d.read("systemctl-calls").contains("--no-block restart jackstayd.service")
    });
}

#[test]
fn what_could_not_be_done_keeps_the_window_open() {
    let d = desk("failing", "", Some(ENTRY));
    ok(std::fs::write(d.root.join("systemctl-fails"), ""));
    d.open();
    d.keys(&["alt+e", "ctrl+a"]);
    d.typed("5");
    d.keys(&["alt+s"]);
    // Saved, but not restarted: said, and left open to say it.
    eventually("the save", || d.config().exists());
    d.stays_open();
    assert!(d.read("systemctl-calls").contains("--no-block restart jackstayd.service"));
    d.keys(&["Escape"]);
    d.closed();
    // A file in the way: nothing saved.
    let _ = std::fs::remove_file(d.config());
    ok(std::fs::create_dir_all(d.config().join("in-the-way")));
    d.open();
    d.keys(&["alt+e", "ctrl+a"]);
    d.typed("6");
    d.keys(&["alt+s"]);
    d.stays_open();
    assert!(d.config().is_dir());
}

#[test]
fn the_hotkey_is_bound_in_xfces_shortcuts_replacing_the_old_one() {
    let popup = popup_command();
    let start = format!(
        "/commands/custom/<Alt>F2 xfce4-appfinder\n/commands/custom/<Primary><Alt>h {popup}\n\
         /xfwm4/custom/<Super>d show_desktop_key\n"
    );
    let d = desk("hotkey", &start, Some(ENTRY));
    d.open();
    d.press("alt+g");
    d.keys(&["ctrl+alt+j", "alt+s"]);
    d.closed();
    assert_eq!(
        d.read("xfconf"),
        format!(
            "/commands/custom/<Alt>F2 xfce4-appfinder\n/xfwm4/custom/<Super>d show_desktop_key\n\
             /commands/custom/<Primary><Alt>j {popup}\n"
        )
    );
    assert_eq!(d.read("systemctl-calls"), "", "no limit changed, no restart");
    assert!(!d.config().exists());
    // Esc stops asking, and nothing changes.
    let before = d.read("xfconf");
    d.open();
    d.press("alt+g");
    d.keys(&["Escape", "alt+s"]);
    d.closed();
    assert_eq!(d.read("xfconf"), before);
    // Keys another command or the window manager has, and keys without a modifier, are refused,
    // still asking: the next keys are taken.
    d.open();
    d.press("alt+g");
    d.keys(&["alt+F2", "super+d", "j", "ctrl+alt+k", "alt+s"]);
    d.closed();
    assert_eq!(
        d.read("xfconf"),
        format!(
            "/commands/custom/<Alt>F2 xfce4-appfinder\n/xfwm4/custom/<Super>d show_desktop_key\n\
             /commands/custom/<Primary><Alt>k {popup}\n"
        )
    );
    // Removed.
    d.open();
    d.press("alt+r");
    d.keys(&["alt+s"]);
    d.closed();
    assert!(!d.read("xfconf").contains(&popup), "{}", d.read("xfconf"));
    assert!(!d.read("xfconf").contains("<Primary><Alt>j"));
    assert!(d.read("xfconf").contains("xfce4-appfinder"), "the user's own binding kept");
}

#[test]
fn starting_at_login_is_the_autostart_entrys_hidden_key() {
    let d = desk("login", "", Some(ENTRY));
    d.open();
    d.press("alt+l");
    d.keys(&["alt+s"]);
    d.closed();
    assert_eq!(
        d.entry(),
        "[Desktop Entry]\nHidden=true\nType=Application\nName=Jackstay\nExec=/b/jackstay-session\n"
    );
    d.open();
    d.press("alt+l");
    d.keys(&["alt+s"]);
    d.closed();
    assert!(d.entry().contains("Hidden=false\n"));
    assert_eq!(d.read("systemctl-calls"), "");
}

#[test]
fn without_xfconf_or_an_autostart_entry_the_limits_still_save() {
    let d = desk("bare", "", None);
    ok(std::fs::write(d.root.join("xfconf-broken"), ""));
    d.open();
    // Neither can be changed: the keys do nothing.
    for mnemonic in ["alt+g", "alt+l", "alt+r"] {
        d.press(mnemonic);
    }
    d.keys(&["ctrl+alt+j"]);
    d.keys(&["alt+e", "ctrl+a"]);
    d.typed("9");
    d.keys(&["alt+s"]);
    d.closed();
    assert_eq!(
        ok(std::fs::read_to_string(d.config())),
        format!("{HEADER}\n[history]\nmax_entries = 9\n")
    );
    assert!(!d.root.join("config/autostart/jackstay.desktop").exists());
    assert_eq!(d.read("xfconf"), "");
}

#[test]
fn asked_again_while_open_the_window_is_raised_not_reread() {
    let d = desk("again", "", Some(ENTRY));
    d.open();
    d.keys(&["alt+e", "ctrl+a"]);
    d.typed("44");
    ok(ui_socket::request(&d.ui.runtime, Ask::Settings, Duration::from_secs(5)));
    std::thread::sleep(Duration::from_millis(300));
    d.keys(&["alt+s"]);
    d.closed();
    assert!(ok(std::fs::read_to_string(d.config())).contains("max_entries = 44\n"));
}

#[test]
fn saving_something_else_leaves_every_binding_alone() {
    let popup = popup_command();
    let two =
        format!("/commands/custom/<Primary><Alt>h {popup}\n/commands/custom/<Super>v {popup}\n");
    let d = desk("bindings", &two, Some(ENTRY));
    d.open();
    d.keys(&["alt+e", "ctrl+a"]);
    d.typed("30");
    d.keys(&["alt+s"]);
    d.closed();
    assert!(d.config().exists());
    assert_eq!(d.read("xfconf"), two);
}

/// Asked for while a save runs, after closing it: shown once the save is done.
#[test]
fn asked_for_during_a_save_the_window_still_opens() {
    let d = desk("during", "", Some(ENTRY));
    ok(std::fs::write(d.root.join("systemctl-slow"), ""));
    d.open();
    d.keys(&["alt+e", "ctrl+a"]);
    d.typed("31");
    d.press("alt+s");
    d.keys(&["Escape"]);
    d.closed();
    ok(ui_socket::request(&d.ui.runtime, Ask::Settings, Duration::from_secs(5)));
    eventually("the save", || d.read("systemctl-calls").contains("restart"));
    eventually("the window again", || d.visible());
}
