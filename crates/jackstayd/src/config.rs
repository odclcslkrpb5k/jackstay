//! `~/.config/jackstay/config.toml` (docs/daemon.md, "Configuration"): every key optional, every
//! value checked, an unknown key refused, so a typo never silently means a default.

use std::fmt::Write as _;
use std::num::NonZeroUsize;

/// The daemon's settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Unpinned entries kept.
    pub max_entries: NonZeroUsize,
    /// Bytes kept across unpinned entries, the newest excluded.
    pub max_bytes: u64,
    /// Pinned entries allowed; pinning beyond is refused.
    pub max_pinned: usize,
    /// The largest single target kept.
    pub per_target: u64,
    /// The most kept from one copy.
    pub per_entry: u64,
    /// Whether the link to other machines runs (docs/link.md).
    pub link_enabled: bool,
    /// The link's UDP port; pairing uses the next one.
    pub link_port: u16,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            max_entries: NonZeroUsize::new(100).unwrap_or(NonZeroUsize::MIN),
            max_bytes: 512 << 20,
            max_pinned: 1000,
            per_target: 64 << 20,
            per_entry: 128 << 20,
            link_enabled: true,
            link_port: jackstay_net::DEFAULT_PORT,
        }
    }
}

/// Why the configuration is refused; names the key, never echoes the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// Not valid TOML.
    Syntax,
    UnknownKey(String),
    /// The value is not a whole number in the key's range.
    BadValue(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Syntax => write!(f, "the configuration is not valid TOML"),
            ConfigError::UnknownKey(k) => write!(f, "unknown configuration key {k}"),
            ConfigError::BadValue(k) => write!(f, "bad value for {k}"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// The largest `history.max_entries`.
pub const MAX_ENTRIES: u64 = 0xFFFF_FFFF;
/// The largest `history.max_bytes`.
pub const MAX_BYTES: u64 = u64::MAX >> 1;
/// The largest `history.max_pinned`.
pub const MAX_PINNED: u64 = 0xFFFF_FFFF;
/// The largest per-target limit: the engine holds a target in memory, and one beyond this is no
/// copy anyone means to keep.
pub const MAX_PER_TARGET: u64 = 1 << 30;
/// The lowest `link.port`: below it, ports are the system's.
pub const MIN_LINK_PORT: u64 = 1024;
/// The highest `link.port`: pairing needs the next port too.
pub const MAX_LINK_PORT: u64 = 65534;
/// The largest `capture.per_entry`.
pub const MAX_PER_ENTRY: u64 = MAX_PER_TARGET << 2;

fn number(value: &toml::Value, key: &str, min: u64, max: u64) -> Result<u64, ConfigError> {
    value
        .as_integer()
        .and_then(|n| u64::try_from(n).ok())
        .filter(|n| (min..=max).contains(n))
        .ok_or_else(|| ConfigError::BadValue(key.to_owned()))
}

/// Parses the file's text.
///
/// # Errors
/// A [`ConfigError`] naming the first key that is unknown or out of range.
pub fn parse(text: &str) -> Result<Config, ConfigError> {
    let table: toml::Table = text.parse().map_err(|_| ConfigError::Syntax)?;
    let mut config = Config::default();
    for (section, value) in &table {
        let Some(keys) = value.as_table() else {
            return Err(ConfigError::UnknownKey(section.clone()));
        };
        for (key, value) in keys {
            let name = format!("{section}.{key}");
            match (section.as_str(), key.as_str()) {
                ("history", "max_entries") => {
                    let n = number(value, &name, 1, MAX_ENTRIES)?;
                    config.max_entries = usize::try_from(n)
                        .ok()
                        .and_then(NonZeroUsize::new)
                        .ok_or(ConfigError::BadValue(name))?;
                }
                ("history", "max_bytes") => {
                    config.max_bytes = number(value, &name, 0, MAX_BYTES)?;
                }
                ("history", "max_pinned") => {
                    let n = number(value, &name, 0, MAX_PINNED)?;
                    config.max_pinned =
                        usize::try_from(n).map_err(|_| ConfigError::BadValue(name))?;
                }
                ("capture", "per_target") => {
                    config.per_target = number(value, &name, 1, MAX_PER_TARGET)?;
                }
                ("capture", "per_entry") => {
                    config.per_entry = number(value, &name, 1, MAX_PER_ENTRY)?;
                }
                ("link", "enabled") => {
                    config.link_enabled =
                        value.as_bool().ok_or_else(|| ConfigError::BadValue(name.clone()))?;
                }
                ("link", "port") => {
                    let n = number(value, &name, MIN_LINK_PORT, MAX_LINK_PORT)?;
                    config.link_port = u16::try_from(n).map_err(|_| ConfigError::BadValue(name))?;
                }
                _ => return Err(ConfigError::UnknownKey(name)),
            }
        }
    }
    if config.per_entry < config.per_target {
        return Err(ConfigError::BadValue("capture.per_entry".to_owned()));
    }
    Ok(config)
}

/// The file's text for `config`, as the settings window writes it: only the keys that differ from
/// their defaults, so a default changed later still applies to the rest. [`parse`] reads it back
/// as `config`.
#[must_use]
pub fn render(config: &Config) -> String {
    let wide = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
    let default = Config::default();
    let keys = [
        ("history", "max_entries", wide(config.max_entries.get()), wide(default.max_entries.get())),
        ("history", "max_bytes", config.max_bytes, default.max_bytes),
        ("history", "max_pinned", wide(config.max_pinned), wide(default.max_pinned)),
        ("capture", "per_target", config.per_target, default.per_target),
        ("capture", "per_entry", config.per_entry, default.per_entry),
    ]
    .map(|(section, key, value, default)| (section, key, value.to_string(), default.to_string()));
    let link = [
        ("link", "enabled", config.link_enabled.to_string(), default.link_enabled.to_string()),
        ("link", "port", config.link_port.to_string(), default.link_port.to_string()),
    ];
    let keys: Vec<_> = keys.into_iter().chain(link).collect();
    let mut text = String::from(
        "# Written by Jackstay's settings window; a key left out has its default (docs/daemon.md).\n",
    );
    let mut section = "";
    for (in_section, key, value, _) in keys.iter().filter(|(.., value, default)| value != default) {
        if *in_section != section {
            section = in_section;
            let _ = write!(text, "\n[{section}]\n");
        }
        let _ = writeln!(text, "{key} = {value}");
    }
    text
}
