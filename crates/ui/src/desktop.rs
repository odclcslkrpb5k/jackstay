//! What the settings window reads and changes (docs/ui.md, "Settings"), without GTK: the daemon's
//! `config.toml`, the popup's hotkey in Xfce's keyboard shortcuts, and whether Jackstay starts at
//! login. Run on a thread of its own, since each step may wait on another program.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use jackstayd::config::{self, Config};

/// Xfce's keyboard shortcuts, where `tools/install --hotkey` binds the popup too.
pub const CHANNEL: &str = "xfce4-keyboard-shortcuts";
/// Commands bound to keys: `/commands/custom/<keys>` holds the command.
pub const COMMANDS: &str = "/commands/custom/";
/// The window manager's actions bound to keys, which a command must not take.
pub const WM_ACTIONS: &str = "/xfwm4/custom/";
const DEFAULT_COMMANDS: &str = "/commands/default/";
const DEFAULT_WM_ACTIONS: &str = "/xfwm4/default/";
/// The daemon's unit, restarted to read a saved configuration.
pub const UNIT: &str = "jackstayd.service";

/// Where each setting lives.
#[derive(Debug, Clone)]
pub struct Places {
    /// The daemon's `config.toml`.
    pub config: PathBuf,
    /// Jackstay's XDG autostart entry, which `tools/install` writes.
    pub autostart: PathBuf,
    /// The command a hotkey runs, as `tools/install` binds it.
    pub popup_command: String,
}

impl Places {
    /// From the environment (`XDG_CONFIG_HOME`, else `~/.config`), with `bin` the directory
    /// holding `jackstay`.
    #[must_use]
    pub fn from_env(env: &dyn Fn(&str) -> Option<OsString>, bin: &Path) -> Option<Places> {
        let config_home = env("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| {
                env("HOME")
                    .map(PathBuf::from)
                    .filter(|p| p.is_absolute())
                    .map(|h| h.join(".config"))
            })?;
        Some(Places {
            config: config_home.join("jackstay").join("config.toml"),
            autostart: config_home.join("autostart").join("jackstay.desktop"),
            popup_command: format!("{} popup", bin.join("jackstay").display()),
        })
    }
}

/// The settings as found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    /// The configuration in force at the daemon's next start: the defaults if there is no file.
    pub config: Config,
    /// Why the file could not be read, if it could not; the defaults are shown, and saving
    /// replaces it.
    pub config_problem: Option<String>,
    /// Every key binding in Xfce's keyboard shortcuts, as (property, value); `None` if they could
    /// not be read (no `xfconf-query`).
    pub shortcuts: Option<Vec<(String, String)>>,
    /// Whether Jackstay starts at login; `None` if it has no autostart entry.
    pub login: Option<bool>,
}

impl Loaded {
    /// The keys bound to the popup, if the shortcuts could be read.
    #[must_use]
    pub fn hotkeys(&self, places: &Places) -> Option<Vec<String>> {
        self.shortcuts.as_ref().map(|s| bound_to(s, &places.popup_command))
    }
}

/// What to change; `None` leaves it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Changes {
    pub config: Option<Config>,
    /// The popup's keys, or none to unbind it.
    pub hotkey: Option<Option<String>>,
    pub login: Option<bool>,
}

/// Reads every setting.
#[must_use]
pub fn load(places: &Places) -> Loaded {
    let (config, config_problem) = match fs::read_to_string(&places.config) {
        Ok(text) => match config::parse(&text) {
            Ok(c) => (c, None),
            Err(e) => (Config::default(), Some(e.to_string())),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => (Config::default(), None),
        Err(e) => (Config::default(), Some(format!("unreadable: {}", e.kind()))),
    };
    let shortcuts = xfconf(&["-c", CHANNEL, "-l", "-v"]).ok().map(|out| listing(&out));
    let login = fs::read_to_string(&places.autostart).ok().map(|t| starts_at_login(&t));
    Loaded { config, config_problem, shortcuts, login }
}

/// Makes `changes`, each independently of the others' failure; returns what went wrong, one line
/// each, or nothing.
#[must_use]
pub fn save(places: &Places, loaded: &Loaded, changes: &Changes) -> Vec<String> {
    let mut problems = Vec::new();
    if let Some(c) = &changes.config {
        match write_file(&places.config, &config::render(c), 0o600) {
            // The daemon reads its configuration only as it starts: restarted, or started if a
            // refused file had stopped it. Not waited for: it saves its history on the way down.
            Ok(()) => {
                if let Err(e) = run("systemctl", &["--user", "--no-block", "restart", UNIT]) {
                    problems.push(format!(
                        "Saved, but jackstayd could not be restarted ({e}); the limits apply when \
                         it next starts."
                    ));
                }
            }
            Err(e) => problems.push(format!("The limits could not be saved: {e}.")),
        }
    }
    if let Some(Err(e)) = changes.hotkey.as_ref().map(|keys| bind(places, loaded, keys.as_deref()))
    {
        problems.push(format!("The hotkey could not be changed: {e}."));
    }
    if let Some(login) = changes.login {
        let saved = fs::read_to_string(&places.autostart)
            .and_then(|t| write_file(&places.autostart, &with_hidden(&t, !login), 0o644));
        if let Err(e) = saved {
            problems.push(format!("Starting at login could not be changed: {}.", e.kind()));
        }
    }
    problems
}

/// Binds the popup to `keys` (first, so a failure part way leaves it bound), then removes its
/// other bindings.
fn bind(places: &Places, loaded: &Loaded, keys: Option<&str>) -> Result<(), String> {
    let old = loaded.hotkeys(places).ok_or("Xfce's keyboard shortcuts could not be read")?;
    if let Some(keys) = keys.filter(|keys| !old.iter().any(|k| k == keys)) {
        let property = format!("{COMMANDS}{keys}");
        let set = ["-c", CHANNEL, "-p", &property, "-n", "-t", "string", "-s"];
        xfconf(&[&set[..], &[places.popup_command.as_str()]].concat())?;
    }
    for k in old.iter().filter(|k| Some(k.as_str()) != keys) {
        xfconf(&["-c", CHANNEL, "-p", &format!("{COMMANDS}{k}"), "-r"])?;
    }
    Ok(())
}

/// `xfconf-query -l -v`'s listing as (property, value): the property, then spaces, then the
/// value, one a line.
#[must_use]
pub fn listing(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|line| {
            let (property, value) = line.split_once(' ')?;
            Some((property.to_owned(), value.trim_start_matches(' ').to_owned()))
        })
        .collect()
}

/// The keys of the commands bound to `command`.
#[must_use]
pub fn bound_to(shortcuts: &[(String, String)], command: &str) -> Vec<String> {
    shortcuts
        .iter()
        .filter(|(_, value)| value == command)
        .filter_map(|(property, _)| property.strip_prefix(COMMANDS))
        .map(str::to_owned)
        .collect()
}

/// What else `keys` runs, if anything: a command, or one of the window manager's actions. `same`
/// says whether two of Xfce's key names mean the same keys (`<Primary>` and `<Control>` do).
///
/// Each group's `custom` bindings are the ones in force once its `custom/override` is true; until
/// then Xfce copies the `default` ones there, so those are checked too.
#[must_use]
pub fn taken(
    shortcuts: &[(String, String)],
    keys: &str,
    command: &str,
    same: impl Fn(&str, &str) -> bool,
) -> Option<String> {
    let overridden = |custom: &str| {
        let flag = format!("{custom}override");
        shortcuts.iter().any(|(p, v)| *p == flag && v == "true")
    };
    let mut groups = vec![(COMMANDS, "runs"), (WM_ACTIONS, "is the window manager's")];
    for (custom, default, kind) in [
        (COMMANDS, DEFAULT_COMMANDS, "runs"),
        (WM_ACTIONS, DEFAULT_WM_ACTIONS, "is the window manager's"),
    ] {
        if !overridden(custom) {
            groups.push((default, kind));
        }
    }
    shortcuts.iter().find_map(|(property, value)| {
        let (kind, other) =
            groups.iter().find_map(|(g, kind)| property.strip_prefix(g).map(|k| (kind, k)))?;
        (other != "override" && same(other, keys) && value != command)
            .then(|| format!("{kind} {value}"))
    })
}

/// The group an autostart entry's keys are in.
const ENTRY: &str = "[Desktop Entry]";

/// Whether an autostart entry starts its program: not `Hidden=true`, as Xfce's Session and
/// Startup reads it.
#[must_use]
pub fn starts_at_login(text: &str) -> bool {
    let mut group = "";
    !text.lines().any(|line| {
        if line.starts_with('[') {
            group = line.trim_end();
        }
        group == ENTRY && hidden_key(line).is_some_and(|v| v == "true")
    })
}

/// The entry with `Hidden` set as Xfce's Session and Startup sets it, first in its group, and
/// everything else kept.
#[must_use]
pub fn with_hidden(text: &str, hidden: bool) -> String {
    let setting = format!("Hidden={hidden}\n");
    let mut out = String::with_capacity(text.len() + setting.len());
    let mut group = "";
    let mut set = false;
    for line in text.lines() {
        if line.starts_with('[') {
            group = line.trim_end();
        } else if group == ENTRY && hidden_key(line).is_some() {
            continue;
        }
        out.push_str(line);
        out.push('\n');
        if group == ENTRY && line.starts_with('[') && !set {
            out.push_str(&setting);
            set = true;
        }
    }
    if set { out } else { format!("{ENTRY}\n{setting}{out}") }
}

/// The value of a `Hidden` line (spaces around `=` allowed, as `GKeyFile` allows them).
fn hidden_key(line: &str) -> Option<&str> {
    let (key, value) = line.split_once('=')?;
    (key.trim() == "Hidden").then(|| value.trim())
}

/// Writes `text` to `path` through a temporary file renamed over it, flushed to disk first, so a
/// reader (or a crash) never sees half of it. Makes the directory if need be; a symbolic link is
/// followed, and the file it names replaced.
///
/// # Errors
/// The I/O error that stopped it; `path` is then as it was.
pub fn write_file(path: &Path, text: &str, mode: u32) -> io::Result<()> {
    // Through a symbolic link (a dotfile manager's), not over it.
    let target = fs::canonicalize(path).ok().filter(|_| path.is_symlink());
    let path = target.as_deref().unwrap_or(path);
    let dir = path.parent().ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    fs::create_dir_all(dir)?;
    let name = path.file_name().ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut tmp_name = name.to_os_string();
    tmp_name.push(format!(".tmp-{}", std::process::id()));
    let tmp = dir.join(tmp_name);
    // One a crash left would otherwise refuse the new one.
    let _ = fs::remove_file(&tmp);
    let written = (|| {
        let mut f = OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        fs::File::open(dir)?.sync_all()
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

/// Runs `xfconf-query` with `args`; its output if it succeeded.
fn xfconf(args: &[&str]) -> Result<String, String> {
    run("xfconf-query", args)
}

fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("{program}: {}", e.kind()))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let said = String::from_utf8_lossy(&out.stderr);
        let said = said.lines().next().unwrap_or_default().trim();
        Err(if said.is_empty() {
            format!("{program} failed")
        } else {
            format!("{program}: {said}")
        })
    }
}
