//! The peers file (docs/link.md, "Peers"): the machines this one is paired with. Written only by
//! the link, whole, and read strictly, so a damaged file stops the link rather than being guessed
//! at.
//!
//! One line per peer: `peer <public key, hex> <address> <name>`; the name is the rest of the line.
//! Lines starting with `#` and empty lines are skipped.

use std::fmt::{self, Write as _};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use jackstay_proto::control::printable;

use crate::address::Address;
use crate::identity::PublicKey;

/// The most peers a machine has.
pub const MAX_PEERS: usize = 8;
/// The most the peers file may hold.
const MAX_FILE_LEN: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub key: PublicKey,
    /// Where to dial it, as the user gave it to `jackstay pair`.
    pub address: Address,
    /// Its hostname as it said at pairing, made printable: for display only.
    pub name: String,
}

#[derive(Debug)]
pub enum PeersError {
    Io(io::Error),
    /// The line (counted from 1) is not a peer, or repeats a key, or is one too many.
    Malformed {
        line: usize,
    },
    /// A peer has this machine's own key (a copied disk).
    OwnKey,
}

impl fmt::Display for PeersError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PeersError::Io(e) => write!(f, "peers file: {e}"),
            PeersError::Malformed { line } => write!(f, "peers file: line {line} is not a peer"),
            PeersError::OwnKey => write!(f, "peers file: lists this machine's own key"),
        }
    }
}

impl std::error::Error for PeersError {}

/// A name as it is kept and shown: printable, one line, no surrounding space, at most 64
/// characters.
#[must_use]
pub fn display_name(name: &str) -> String {
    printable(name).trim().chars().take(64).collect()
}

/// The peers in `path`; none if there is no file.
///
/// # Errors
/// A [`PeersError`] if the file cannot be read or any line is not a peer.
pub fn load(path: &Path) -> Result<Vec<Peer>, PeersError> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(PeersError::Io(e)),
    };
    let mut bytes = Vec::new();
    file.take(MAX_FILE_LEN.saturating_add(1)).read_to_end(&mut bytes).map_err(PeersError::Io)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_FILE_LEN {
        return Err(PeersError::Malformed { line: 0 });
    }
    let text = String::from_utf8(bytes).map_err(|_| PeersError::Malformed { line: 0 })?;
    parse(&text)
}

/// The peers in `text`, as [`load`] reads them.
///
/// # Errors
/// [`PeersError::Malformed`] naming the first line that is not a peer.
pub fn parse(text: &str) -> Result<Vec<Peer>, PeersError> {
    let mut peers: Vec<Peer> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let number = n.saturating_add(1);
        let bad = PeersError::Malformed { line: number };
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.splitn(4, ' ');
        let (Some("peer"), Some(key), Some(address), Some(name)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(bad);
        };
        let key = PublicKey::from_hex(key).ok_or(PeersError::Malformed { line: number })?;
        let address = Address::parse(address).ok_or(PeersError::Malformed { line: number })?;
        if name != display_name(name) || peers.iter().any(|p| p.key == key) {
            return Err(bad);
        }
        if peers.len() >= MAX_PEERS {
            return Err(bad);
        }
        peers.push(Peer { key, address, name: name.to_owned() });
    }
    Ok(peers)
}

/// `peers` as the file holds them.
#[must_use]
pub fn render(peers: &[Peer]) -> String {
    let mut text =
        "# Jackstay's paired machines. Written by jackstayd: use `jackstay pair` and `unpair`.\n"
            .to_owned();
    for p in peers {
        let _ = writeln!(text, "peer {} {} {}", p.key.to_hex(), p.address, p.name);
    }
    text
}

/// Replaces the file at `path` with `peers`: a temporary file, synced, renamed over it.
///
/// # Errors
/// The I/O error that stopped it; the old file is then unchanged.
pub fn save(path: &Path, peers: &[Peer]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let tmp = dir.join(format!(".peers.{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    let written = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
            .open(&tmp)?;
        file.write_all(render(peers).as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        File::open(dir)?.sync_all()
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}
