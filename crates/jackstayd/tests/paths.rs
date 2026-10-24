use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use jackstayd::paths::{Dirs, Overrides, PathError, lock, private_dir, resolve};

fn env<'a>(vars: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
    move |name| vars.iter().find(|(k, _)| *k == name).map(|(_, v)| OsString::from(v))
}

fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
}

#[test]
fn xdg_variables_are_used_and_relative_ones_ignored() {
    let vars = [
        ("HOME", "/home/u"),
        ("XDG_RUNTIME_DIR", "/run/user/1000"),
        ("XDG_STATE_HOME", "/s"),
        ("XDG_CONFIG_HOME", "relative/config"),
        ("XDG_CACHE_HOME", ""),
    ];
    let d = resolve(&env(&vars), &Overrides::default());
    assert_eq!(
        d,
        Ok(Dirs {
            state: p("/s/jackstay"),
            runtime: p("/run/user/1000/jackstay"),
            config: p("/home/u/.config/jackstay/config.toml"),
            cache_home: p("/home/u/.cache"),
        })
    );
    let defaults =
        resolve(&env(&[("HOME", "/h"), ("XDG_RUNTIME_DIR", "/r")]), &Overrides::default());
    assert_eq!(defaults.map(|d| d.state), Ok(p("/h/.local/state/jackstay")));
}

#[test]
fn a_runtime_directory_is_required_and_flags_win() {
    assert_eq!(
        resolve(&env(&[("HOME", "/h")]), &Overrides::default()),
        Err(PathError::NoRuntimeDir)
    );
    assert_eq!(
        resolve(&env(&[("HOME", "/h"), ("XDG_RUNTIME_DIR", "run")]), &Overrides::default()),
        Err(PathError::NoRuntimeDir)
    );
    assert_eq!(
        resolve(&env(&[("XDG_RUNTIME_DIR", "/r")]), &Overrides::default()),
        Err(PathError::NoHome)
    );
    let over =
        Overrides { state: Some(p("/a")), runtime: Some(p("/b")), config: Some(p("/c.toml")) };
    let d = resolve(&env(&[("XDG_CACHE_HOME", "/cache")]), &over);
    assert_eq!(
        d,
        Ok(Dirs {
            state: p("/a"),
            runtime: p("/b"),
            config: p("/c.toml"),
            cache_home: p("/cache")
        })
    );
}

fn scratch(name: &str) -> PathBuf {
    let dir =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("paths-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn private_directories_are_made_private_and_links_refused() {
    let dir = scratch("private");
    assert_eq!(private_dir(&dir.join("a/b")), Ok(()));
    let mode = |d: &Path| std::fs::metadata(d).map_or(0, |m| m.permissions().mode() & 0o777);
    assert_eq!(mode(&dir.join("a/b")), 0o700);
    let open = dir.join("open");
    let _ = std::fs::create_dir(&open);
    let _ = std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755));
    assert_eq!(private_dir(&open), Ok(()));
    assert_eq!(mode(&open), 0o700, "tightened");
    let inverted = dir.join("inverted");
    let _ = std::fs::create_dir(&inverted);
    let _ = std::fs::set_permissions(&inverted, std::fs::Permissions::from_mode(0o077));
    assert_eq!(private_dir(&inverted), Ok(()));
    assert_eq!(mode(&inverted), 0o700);
    let link = dir.join("link");
    let _ = std::os::unix::fs::symlink(&open, &link);
    assert_eq!(private_dir(&link), Err(PathError::NotPrivate(link.clone())));
    let file = dir.join("file");
    let _ = std::fs::write(&file, b"");
    assert!(private_dir(&file).is_err());
    assert_eq!(
        PathError::NoRuntimeDir.to_string(),
        "XDG_RUNTIME_DIR is not set to an absolute path"
    );
}

#[test]
fn the_instance_lock_is_exclusive() {
    let dir = scratch("lock");
    assert_eq!(private_dir(&dir), Ok(()));
    let first = lock(&dir).unwrap_or_else(|e| panic!("{e}"));
    assert!(first.is_some());
    assert!(lock(&dir).unwrap_or_else(|e| panic!("{e}")).is_none(), "held");
    drop(first);
    assert!(lock(&dir).unwrap_or_else(|e| panic!("{e}")).is_some(), "released with the file");
}
