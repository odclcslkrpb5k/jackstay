//! `~/.config/jackstay/config.toml` (docs/daemon.md, "Configuration"): every key optional, every
//! value checked, an unknown key refused, so a typo never silently means a default.

use std::num::NonZeroUsize;

/// The daemon's settings, all in entries or bytes.
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
}

impl Default for Config {
    fn default() -> Self {
        Config {
            max_entries: NonZeroUsize::new(100).unwrap_or(NonZeroUsize::MIN),
            max_bytes: 512 << 20,
            max_pinned: 1000,
            per_target: 64 << 20,
            per_entry: 128 << 20,
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

/// The largest per-target limit: the engine holds a target in memory, and one beyond this is no
/// copy anyone means to keep.
const MAX_PER_TARGET: u64 = 1 << 30;

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
                    let n = number(value, &name, 1, u64::from(u32::MAX))?;
                    config.max_entries = usize::try_from(n)
                        .ok()
                        .and_then(NonZeroUsize::new)
                        .ok_or(ConfigError::BadValue(name))?;
                }
                ("history", "max_bytes") => {
                    config.max_bytes = number(value, &name, 0, u64::MAX >> 1)?;
                }
                ("history", "max_pinned") => {
                    let n = number(value, &name, 0, u64::from(u32::MAX))?;
                    config.max_pinned =
                        usize::try_from(n).map_err(|_| ConfigError::BadValue(name))?;
                }
                ("capture", "per_target") => {
                    config.per_target = number(value, &name, 1, MAX_PER_TARGET)?;
                }
                ("capture", "per_entry") => {
                    config.per_entry = number(value, &name, 1, MAX_PER_TARGET << 2)?;
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
