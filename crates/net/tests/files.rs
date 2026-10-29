//! The link's files and names: the identity, the peers file, addresses.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use jackstay_net::identity::PUBLIC_KEY_LEN;
use jackstay_net::peers::{self, MAX_PEERS, Peer};
use jackstay_net::{Address, Identity, IdentityError, PeersError, PublicKey};

fn scratch(name: &str) -> PathBuf {
    let dir =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("net-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
    dir
}

#[test]
fn an_identity_is_made_once_then_read_back() {
    let dir = scratch("identity");
    let path = dir.join("identity");
    let first = Identity::load_or_create(&path).unwrap_or_else(|e| panic!("{e}"));
    let mode = std::fs::metadata(&path).unwrap_or_else(|e| panic!("{e}")).permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let again = Identity::load_or_create(&path).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(first.public(), again.public());
    // No temporary file is left behind.
    let names: Vec<_> = std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("{e}")).collect();
    assert_eq!(names.len(), 1);
}

#[test]
fn an_identity_readable_by_others_is_made_private() {
    let dir = scratch("identity-mode");
    let path = dir.join("identity");
    let made = Identity::load_or_create(&path).unwrap_or_else(|e| panic!("{e}")).public();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
        .unwrap_or_else(|e| panic!("{e}"));
    let read = Identity::load_or_create(&path).unwrap_or_else(|e| panic!("{e}")).public();
    assert_eq!(read, made);
    let after = std::fs::metadata(&path).unwrap_or_else(|e| panic!("{e}")).permissions().mode();
    assert_eq!(after & 0o777, 0o600);
}

#[test]
fn two_machines_have_different_keys() {
    let a = Identity::load_or_create(&scratch("key-a").join("identity"))
        .unwrap_or_else(|e| panic!("{e}"));
    let b = Identity::load_or_create(&scratch("key-b").join("identity"))
        .unwrap_or_else(|e| panic!("{e}"));
    assert_ne!(a.public(), b.public());
    assert_ne!(a.public().fingerprint(), b.public().fingerprint());
}

#[test]
fn a_damaged_identity_is_refused_and_kept() {
    let dir = scratch("identity-bad");
    let path = dir.join("identity");
    std::fs::write(&path, b"not a key").unwrap_or_else(|e| panic!("{e}"));
    assert!(matches!(Identity::load_or_create(&path), Err(IdentityError::Malformed)));
    // Never silently replaced: that would unpair every peer.
    assert_eq!(std::fs::read(&path).unwrap_or_else(|e| panic!("{e}")), b"not a key");
}

#[test]
fn an_identity_behind_a_symbolic_link_is_refused() {
    let dir = scratch("identity-link");
    let real = dir.join("real");
    Identity::load_or_create(&real).unwrap_or_else(|e| panic!("{e}"));
    let link = dir.join("identity");
    std::os::unix::fs::symlink(&real, &link).unwrap_or_else(|e| panic!("{e}"));
    assert!(matches!(Identity::load_or_create(&link), Err(IdentityError::Io(_))));
}

#[test]
fn public_keys_are_ed25519_spkis_only() {
    let key = Identity::load_or_create(&scratch("spki").join("identity"))
        .unwrap_or_else(|e| panic!("{e}"))
        .public();
    assert_eq!(PublicKey::from_spki(key.as_bytes()), Some(key));
    assert_eq!(PublicKey::from_hex(&key.to_hex()), Some(key));
    // Another algorithm's prefix, or another length, is not a key.
    let mut other = *key.as_bytes();
    other[8] = 0x71; // Ed448's OID ends 0x71.
    assert_eq!(PublicKey::from_spki(&other), None);
    assert_eq!(PublicKey::from_spki(&key.as_bytes()[..PUBLIC_KEY_LEN - 1]), None);
    assert_eq!(PublicKey::from_hex(&key.to_hex()[1..]), None);
    assert_eq!(PublicKey::from_hex(&key.to_hex().replace(|c: char| c.is_ascii_digit(), "g")), None);
    assert_eq!(key.fingerprint().len(), 16);
}

fn key(seed: u8) -> PublicKey {
    let mut spki = vec![0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];
    spki.extend([seed; 32]);
    PublicKey::from_spki(&spki).unwrap_or_else(|| panic!("not a key"))
}

fn peer(seed: u8, address: &str, name: &str) -> Peer {
    Peer {
        key: key(seed),
        address: Address::parse(address).unwrap_or_else(|| panic!("{address}")),
        name: name.to_owned(),
    }
}

#[test]
fn the_peers_file_round_trips() {
    let all = vec![
        peer(1, "xiphos", "xiphos"),
        peer(2, "192.168.4.60:24900", "the other one"),
        peer(3, "[fd07::1]:24810", ""),
        peer(4, "fd07::2", "v6"),
    ];
    assert_eq!(peers::parse(&peers::render(&all)).map_err(|e| e.to_string()), Ok(all.clone()));
    let path = scratch("peers").join("peers");
    peers::save(&path, &all).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(peers::load(&path).map_err(|e| e.to_string()), Ok(all));
    let mode = std::fs::metadata(&path).unwrap_or_else(|e| panic!("{e}")).permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn no_peers_file_is_no_peers() {
    let path = scratch("peers-none").join("peers");
    assert_eq!(peers::load(&path).map_err(|e| e.to_string()), Ok(Vec::new()));
}

#[test]
fn a_damaged_peers_file_names_the_line() {
    let good = peers::render(&[peer(1, "xiphos", "xiphos")]);
    let line = good.lines().nth(1).unwrap_or_default().to_owned();
    for (text, at) in [
        (format!("{good}nonsense\n"), 3),
        (format!("{good}{line}\n"), 3),       // the same key twice
        (good.replace("peer ", "peer  "), 2), // an empty key
        (good.replace("xiphos xiphos", "xiphos"), 2), // no name field
        (good.replace("xiphos xiphos", "xiphos:0 xiphos"), 2), // port 0
        (good.replace("xiphos xiphos", "xiphos a\tb"), 2), // an unprintable name
        (good.replace("xiphos xiphos", "xi/phos xiphos"), 2), // not a host name
    ] {
        match peers::parse(&text) {
            Err(PeersError::Malformed { line }) => assert_eq!(line, at, "{text}"),
            other => panic!("{text}: {other:?}"),
        }
    }
}

#[test]
fn at_most_the_limit_of_peers_is_read() {
    let all: Vec<Peer> =
        (0..=u8::try_from(MAX_PEERS).unwrap_or(0)).map(|i| peer(i, "h", "n")).collect();
    assert!(peers::parse(&peers::render(&all[..MAX_PEERS])).is_ok());
    assert!(matches!(peers::parse(&peers::render(&all)), Err(PeersError::Malformed { .. })));
}

#[test]
fn names_are_shown_printable_and_short() {
    assert_eq!(peers::display_name("  xiphos\n"), "xiphos");
    assert_eq!(peers::display_name("a\u{1b}[2Jb"), "a [2Jb");
    assert_eq!(peers::display_name(&"x".repeat(100)).len(), 64);
}

#[test]
fn addresses_parse_as_written() {
    for (text, port) in [
        ("xiphos", 24810),
        ("xiphos.local", 24810),
        ("xiphos:24900", 24900),
        ("192.168.4.60", 24810),
        ("192.168.4.60:7", 7),
        ("fd07::1", 24810),
        ("[fd07::1]", 24810),
        ("[fd07::1]:9", 9),
    ] {
        let a = Address::parse(text).unwrap_or_else(|| panic!("{text}"));
        assert_eq!(a.port_or(24810), port, "{text}");
        assert_eq!(Address::parse(&a.to_string()), Some(a), "{text}");
    }
    for text in [
        "",
        ":24810",
        "xiphos:",
        "xiphos:0",
        "xiphos:65536",
        "xiphos:+1",
        "a b",
        "[fd07::1",
        "[x]:1",
        "x/y",
    ] {
        assert_eq!(Address::parse(text), None, "{text:?}");
    }
    assert_eq!(Address::parse(&"a".repeat(255)), None);
}
