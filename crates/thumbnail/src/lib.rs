//! Thumbnails of clipboard images (docs/ui.md, "Thumbnails"). The bytes come from any application
//! on the desktop, so they are decoded only here, by pure-Rust decoders, and only in a helper
//! process that has [`confine`]d itself first: a decoder that crashes, loops or allocates without
//! bound costs that process, never the daemon. gdk-pixbuf never sees them; the UI gets the pixels.
//!
//! The helper is `jackstayd --thumbnail`: the image on its standard input, the encoded
//! [`Thumbnail`] on its standard output ([`helper`]).
#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::string_slice,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]

use std::io::{Read, Write};

use image::{ImageFormat, ImageReader, Limits};
use jackstay_proto::control::{MAX_THUMBNAIL_HEIGHT, MAX_THUMBNAIL_WIDTH, Thumbnail};
use rustix::process::{Resource, Rlimit, setrlimit};

/// The most a decoder may allocate at once. An 8K screenshot needs 133 MiB as RGBA.
pub const MAX_DECODE_ALLOC: u64 = 256 << 20;
/// The helper's address space: the decoder's allocations, the input and the program.
pub const MAX_ADDRESS_SPACE: u64 = 1 << 30;
/// The helper's processor time, in seconds; the daemon also stops waiting after
/// [`HELPER_DEADLINE_MS`].
pub const MAX_CPU_SECONDS: u64 = 5;
/// How long the daemon waits for the helper, in milliseconds.
pub const HELPER_DEADLINE_MS: u64 = 10_000;

/// The formats decoded. Any other is not looked at.
const FORMATS: [ImageFormat; 5] =
    [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::WebP, ImageFormat::Gif, ImageFormat::Bmp];

/// Why no thumbnail was made. Callers branch on the variant (ER-8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Not one of the formats decoded, by its first bytes.
    NotAnImage,
    /// It claimed a format but did not decode: malformed, or past a limit.
    Undecodable,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Error::NotAnImage => "not an image in a format decoded",
            Error::Undecodable => "the image did not decode",
        })
    }
}

impl std::error::Error for Error {}

/// The size `width` by `height` scales to, to fit the thumbnail's limits: the aspect kept, never
/// scaled up, neither side below 1.
#[must_use]
pub fn fit(width: u32, height: u32) -> (u16, u16) {
    let (w, h) = (u64::from(width.max(1)), u64::from(height.max(1)));
    let (max_w, max_h) = (u64::from(MAX_THUMBNAIL_WIDTH), u64::from(MAX_THUMBNAIL_HEIGHT));
    // Wider than the box's aspect: the width decides.
    let (nw, nh) = if w.saturating_mul(max_h) >= h.saturating_mul(max_w) {
        let nw = w.min(max_w);
        (nw, h.saturating_mul(nw).checked_div(w).unwrap_or(1))
    } else {
        let nh = h.min(max_h);
        (w.saturating_mul(nh).checked_div(h).unwrap_or(1), nh)
    };
    let side = |n: u64, max: u16| u16::try_from(n.clamp(1, u64::from(max))).unwrap_or(max);
    (side(nw, MAX_THUMBNAIL_WIDTH), side(nh, MAX_THUMBNAIL_HEIGHT))
}

/// Decodes `bytes` and scales the image down to a thumbnail. Run it only in a confined process.
///
/// # Errors
/// [`Error::NotAnImage`] if its format is not one decoded; [`Error::Undecodable`] if it does not
/// decode within the limits.
pub fn thumbnail(bytes: &[u8]) -> Result<Thumbnail, Error> {
    let format = image::guess_format(bytes).map_err(|_| Error::NotAnImage)?;
    if !FORMATS.contains(&format) {
        return Err(Error::NotAnImage);
    }
    let mut reader = ImageReader::with_format(std::io::Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let image = reader.decode().map_err(|_| Error::Undecodable)?;
    let (width, height) = (image.width(), image.height());
    let (nw, nh) = fit(width, height);
    let small = if (u32::from(nw), u32::from(nh)) == (width, height) {
        image
    } else {
        image.thumbnail_exact(u32::from(nw), u32::from(nh))
    };
    let rgba = small.into_rgba8().into_raw();
    Thumbnail::new(nw, nh, width, height, rgba).map_err(|_| Error::Undecodable)
}

/// Confines this process before it reads anything (docs/daemon.md, "Thumbnails"): bounded memory
/// and processor time; then a seccomp filter that kills it at any system call but reading its
/// standard input, writing its standard output or error, managing its own memory, and exiting. So
/// a decoder made to run code of an attacker's can still not open, change or delete a file, start
/// a process, signal one, or reach a socket or any descriptor it inherited.
///
/// # Errors
/// A limit or the filter could not be set; the helper must then not decode.
pub fn confine() -> std::io::Result<()> {
    let limit = |resource: Resource, n: u64| {
        // Never above a hard limit already lower: raising one is refused.
        let hard = rustix::process::getrlimit(resource).maximum;
        let n = hard.map_or(n, |h| h.min(n));
        setrlimit(resource, Rlimit { current: Some(n), maximum: Some(n) })
    };
    limit(Resource::As, MAX_ADDRESS_SPACE)?;
    limit(Resource::Cpu, MAX_CPU_SECONDS)?;
    limit(Resource::Fsize, 0)?;
    limit(Resource::Core, 0)?;
    limit(Resource::Nproc, 0)?;
    limit(Resource::Nofile, 3)?;
    rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)?;
    // Required before an unprivileged process may install a filter.
    rustix::thread::set_no_new_privs(true)?;
    let filter = filter().map_err(std::io::Error::other)?;
    seccompiler::apply_filter(&filter).map_err(std::io::Error::other)
}

/// The system calls the helper may make once confined, each but `read` and `write` with any
/// arguments. Found by running every format through it under strace; anything else kills it.
const ALLOWED: [i64; 13] = [
    libc::SYS_mmap,
    libc::SYS_munmap,
    libc::SYS_mremap,
    libc::SYS_mprotect,
    libc::SYS_brk,
    libc::SYS_madvise,
    libc::SYS_futex,
    libc::SYS_getrandom,
    libc::SYS_sigaltstack,
    libc::SYS_rt_sigprocmask,
    libc::SYS_rt_sigreturn,
    libc::SYS_exit,
    libc::SYS_exit_group,
];

fn filter() -> Result<seccompiler::BpfProgram, seccompiler::BackendError> {
    use seccompiler::{
        SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule, TargetArch,
    };
    let fd_is = |fd: u64| -> Result<SeccompRule, seccompiler::BackendError> {
        SeccompRule::new(vec![SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Eq,
            fd,
        )?])
    };
    let mut rules: std::collections::BTreeMap<i64, Vec<SeccompRule>> =
        ALLOWED.iter().map(|&call| (call, Vec::new())).collect();
    rules.insert(libc::SYS_read, vec![fd_is(0)?]);
    rules.insert(libc::SYS_write, vec![fd_is(1)?, fd_is(2)?]);
    let arch = TargetArch::try_from(std::env::consts::ARCH)?;
    SeccompFilter::new(rules, SeccompAction::KillProcess, SeccompAction::Allow, arch)?.try_into()
}

/// Exit statuses of [`helper`].
pub mod status {
    pub const MADE: i32 = 0;
    pub const NO_THUMBNAIL: i32 = 1;
    /// Could not confine itself, read its input or write its output.
    pub const FAILED: i32 = 2;
}

/// The helper: confines itself, reads the image from `input` to its end, and writes the encoded
/// thumbnail to `output`. Returns its exit status.
pub fn helper(input: &mut impl Read, output: &mut impl Write) -> i32 {
    if confine().is_err() {
        return status::FAILED;
    }
    let mut bytes = Vec::new();
    if input.read_to_end(&mut bytes).is_err() {
        return status::FAILED;
    }
    match thumbnail(&bytes) {
        Ok(t) => {
            let mut out = Vec::with_capacity(Thumbnail::MAX_ENCODED_LEN);
            t.encode(&mut out);
            if output.write_all(&out).and_then(|()| output.flush()).is_err() {
                return status::FAILED;
            }
            status::MADE
        }
        Err(_) => status::NO_THUMBNAIL,
    }
}
