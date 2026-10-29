//! This machine's Ed25519 key pair, and the public keys that name machines (docs/link.md,
//! "Identity").

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::sync::Arc;

use ring::digest::{SHA256, digest};
use ring::hkdf;
use ring::rand::SystemRandom;
use ring::signature::Ed25519KeyPair;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use rustls::sign::CertifiedKey;

/// The DER prefix of an Ed25519 `SubjectPublicKeyInfo` (RFC 8410): the 32-byte key follows.
const SPKI_PREFIX: [u8; 12] =
    [0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];
/// The length of a public key as it travels: an Ed25519 `SubjectPublicKeyInfo`.
pub const PUBLIC_KEY_LEN: usize = 44;
/// The most an identity file may hold; ring's PKCS#8 for Ed25519 is 83 bytes.
const MAX_IDENTITY_LEN: u64 = 1024;

/// A machine's public key, as its `SubjectPublicKeyInfo`: the machine's only identity. Ordered
/// bytewise, which decides which of two machines' sessions is kept (docs/link.md, "Who dials").
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PublicKey([u8; PUBLIC_KEY_LEN]);

impl PublicKey {
    /// The key in `spki`, if it is an Ed25519 `SubjectPublicKeyInfo`.
    #[must_use]
    pub fn from_spki(spki: &[u8]) -> Option<PublicKey> {
        let bytes: [u8; PUBLIC_KEY_LEN] = spki.try_into().ok()?;
        bytes.starts_with(&SPKI_PREFIX).then_some(PublicKey(bytes))
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8; PUBLIC_KEY_LEN] {
        &self.0
    }

    /// The first 8 bytes of the key's SHA-256, in hex: what a person compares and types.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        hex(digest(&SHA256, &self.0).as_ref().get(..8).unwrap_or_default())
    }

    /// The whole key in hex, as the peers file keeps it.
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }

    /// The key in `text`, as [`PublicKey::to_hex`] writes it.
    #[must_use]
    pub fn from_hex(text: &str) -> Option<PublicKey> {
        if text.len() != PUBLIC_KEY_LEN * 2 {
            return None;
        }
        let mut bytes = Vec::with_capacity(PUBLIC_KEY_LEN);
        for pair in text.as_bytes().as_chunks::<2>().0 {
            let s = std::str::from_utf8(pair).ok()?;
            bytes.push(u8::from_str_radix(s, 16).ok()?);
        }
        PublicKey::from_spki(&bytes)
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({})", self.fingerprint())
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// This machine's key pair. Its Debug shows the public key only.
pub struct Identity {
    certified: Arc<CertifiedKey>,
    public: PublicKey,
    /// Input to the derived keys: the PKCS#8 document, which holds the private key.
    secret: Vec<u8>,
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity").field("public", &self.public).finish_non_exhaustive()
    }
}

/// Why the identity file could not be used. The link stays off; the file is never replaced.
#[derive(Debug)]
pub enum IdentityError {
    Io(io::Error),
    /// The file is not an Ed25519 key in PKCS#8.
    Malformed,
}

impl fmt::Display for IdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdentityError::Io(e) => write!(f, "identity file: {e}"),
            IdentityError::Malformed => write!(f, "identity file is not an Ed25519 key"),
        }
    }
}

impl std::error::Error for IdentityError {}

impl From<io::Error> for IdentityError {
    fn from(e: io::Error) -> Self {
        IdentityError::Io(e)
    }
}

/// The key a derivation is for (docs/link.md, "Identity").
#[derive(Debug, Clone, Copy)]
pub(crate) enum Derived {
    StatelessReset,
    ConnectionId,
}

impl Identity {
    /// Reads the identity at `path`, or makes one there if there is none.
    ///
    /// A new key is written to a temporary file, synced, and linked into place, which fails if the
    /// name exists: then the file that won is read instead.
    ///
    /// # Errors
    /// An [`IdentityError`] if the file cannot be read, made, or used as a key.
    pub fn load_or_create(path: &Path) -> Result<Identity, IdentityError> {
        match read(path) {
            Ok(bytes) => return Identity::from_pkcs8(bytes),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .map_err(|_| io::Error::other("no randomness"))?;
        let dir = path.parent().ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        let tmp = dir.join(format!(".identity.{}", std::process::id()));
        let _ = fs::remove_file(&tmp);
        let written = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
                .open(&tmp)?;
            file.write_all(pkcs8.as_ref())?;
            file.sync_all()?;
            match fs::hard_link(&tmp, path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
            File::open(dir)?.sync_all()
        })();
        let _ = fs::remove_file(&tmp);
        written?;
        Identity::from_pkcs8(read(path)?)
    }

    fn from_pkcs8(bytes: Vec<u8>) -> Result<Identity, IdentityError> {
        Ed25519KeyPair::from_pkcs8(&bytes).map_err(|_| IdentityError::Malformed)?;
        let der = PrivatePkcs8KeyDer::from(bytes.clone());
        let signer = rustls::crypto::ring::sign::any_eddsa_type(&der)
            .map_err(|_| IdentityError::Malformed)?;
        let spki = signer.public_key().ok_or(IdentityError::Malformed)?;
        let public = PublicKey::from_spki(spki.as_ref()).ok_or(IdentityError::Malformed)?;
        let certified =
            Arc::new(CertifiedKey::new(vec![CertificateDer::from(spki.as_ref().to_vec())], signer));
        Ok(Identity { certified, public, secret: bytes })
    }

    #[must_use]
    pub fn public(&self) -> PublicKey {
        self.public
    }

    pub(crate) fn certified(&self) -> Arc<CertifiedKey> {
        Arc::clone(&self.certified)
    }

    /// A 32-byte key derived from the private key by HKDF-SHA256: the same on every start, and
    /// different for each purpose.
    pub(crate) fn derive(&self, purpose: Derived) -> [u8; 32] {
        let info: &[u8] = match purpose {
            Derived::StatelessReset => b"jackstay stateless reset",
            Derived::ConnectionId => b"jackstay connection id",
        };
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, b"jackstay link").extract(&self.secret);
        let mut out = [0u8; 32];
        let info = [info];
        // HKDF-SHA256 can expand to 255 * 32 bytes, so 32 never fails.
        if let Ok(okm) = prk.expand(&info, hkdf::HKDF_SHA256) {
            let _ = okm.fill(&mut out);
        }
        out
    }
}

/// Reads the key, first taking away any access beyond its owner's: a file copied in by hand with
/// a looser mode is made 0600 rather than refused or used as it is.
fn read(path: &Path) -> io::Result<Vec<u8>> {
    use std::os::unix::fs::PermissionsExt as _;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(path)?;
    if file.metadata()?.permissions().mode() & 0o077 != 0 {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    let mut bytes = Vec::new();
    file.take(MAX_IDENTITY_LEN).read_to_end(&mut bytes)?;
    Ok(bytes)
}
