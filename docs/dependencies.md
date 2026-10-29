# Dependencies

Every dependency is a review finding until it is justified here (TC-7). Versions are exact (`=`)
and `Cargo.lock` is committed; builds run `--locked`.

| Crate | Version | Used by | Why | Alternatives rejected |
|---|---|---|---|---|
| `libfuzzer-sys` | 0.4.13 | the fuzz crates only (`crates/*/fuzz`) | libFuzzer for cargo-fuzz (TS-16); its bundled libFuzzer is NCSA, allowed for this crate alone in `deny.toml` | `afl` (needs an instrumenting toolchain outside cargo); `honggfuzz` (less maintained bindings) |
| `x11rb` | 0.14.0 | `jackstay-x11-clip`; `jackstayd` (tests: who owns a selection); `jackstay-ui` (tests: the screen's pixels) | The X protocol in pure Rust (`RustConnection`, no libxcb): the clipboard path has no C (plan §4.3). Features `xfixes` and `res` (X-Resource, for an owner's process): code generated into the same crate, no new ones | `xcb` crate (FFI to libxcb); `x11` (Xlib FFI) |
| `rustix` | 1.1.5 | `jackstay-x11-clip`, `jackstayd`, `jackstay-thumbnail`, `jackstay-net` | `poll` for the daemon's loop, `flock` for the instance lock, `SO_PEERCRED` on the control socket, `O_NOFOLLOW`, `getuid`, `memfd_create` for an image handed to the thumbnail helper, and the helper's confinement (`setrlimit`, `no_new_privs`, not dumpable; feature `thread`), and the link's boot and monotonic clocks, whose drift shows a resume from suspend (feature `time`); already in the tree through `x11rb`, at the same version | `libc` (unsafe FFI in our code); `mio` (more than a poll) |
| `sha2` | 0.11.1 | `jackstay-store` | Names each history blob by the SHA-256 of its bytes, so equal content is stored once and a blob can be checked against its name (docs/daemon.md, "Storage"). RustCrypto, pure Rust, no default features; brings `digest`, `block-buffer`, `crypto-common`, `hybrid-array`, `typenum`, `cpufeatures` (`cpuid` through `core::arch`, no libc on x86-64) and `cfg-if`, all RustCrypto's or long-standing | `ring` (C and assembly; it arrived later with the link's TLS, but the store needs only a hash); a hand-written SHA-256 (unreviewed crypto); a non-cryptographic hash (a chosen collision would let one copy's blob stand for another's) |
| `toml` | 1.1.8 | `jackstayd` | Reads `config.toml` into a table; features `std`, `parse`, `serde` (for `serde_core`'s traits, no derive macros); brings `toml_parser`, `winnow`, `toml_datetime`, `serde_spanned`, `serde_core` | `serde` with derive (proc macros in the build for five keys); a hand-written parser (a second TOML dialect for users to learn) |
| `quinn` | 0.11.12 | `jackstay-net` | QUIC for the link (plan §4.3, docs/link.md): one UDP port, a stream per purpose with priorities, keep-alive and idle timeout, stateless reset, address validation (`retry`). Features `runtime-tokio` and `rustls-ring` only. With `quinn-udp`, `bytes`, `slab`, `lru-slab`, `rustc-hash`, `tinyvec`, `thiserror`, `tracing`, `rand` and its parts, `getrandom` | TCP with TLS (one stream: a clipboard transfer would hold up keypresses); `s2n-quic` (aws-lc, larger); `quiche` (C++ BoringSSL) |
| `quinn-proto` | 0.11.19 | `jackstay-net` | quinn's protocol core, named directly for `HashedConnectionIdGenerator::from_key`: connection IDs derived from the identity, so a restarted daemon recognises and resets its old sessions; already in the graph through `quinn` | quinn's random default (a restarted peer waits out the idle timeout) |
| `rustls` | 0.23.46 | `jackstay-net` | TLS 1.3 for QUIC, with RFC 7250 raw public keys on both ends and a verifier of our own (measured in `spikes/link-probe`). Features `ring`, `std`; no TLS 1.2, no aws-lc. Brings `rustls-pki-types`, `rustls-webpki` (ISC), `untrusted` (ISC), `subtle` (BSD-3-Clause), `zeroize`, `once_cell` | OpenSSL bindings (C, and no raw public keys in the Rust bindings); aws-lc-rs as provider (a CMake build of a large C library) |
| `ring` | 0.17.14 | `jackstay-net` | The TLS provider; also Ed25519 key generation, HKDF for the derived keys, SHA-256 for pairing, and randomness. ISC and Apache-2.0, allowed for this crate alone in `deny.toml`. C and assembly, built by the container's compiler: the one C dependency the link brings, used by most of the Rust TLS ecosystem | aws-lc-rs (larger C, CMake); RustCrypto for each primitive (several crates, and rustls's provider for it is third-party) |
| `tokio` | 1.54.1 | `jackstay-net` | quinn's runtime: one current-thread runtime on the link thread. Features `rt`, `net`, `time`, `sync`, `macros`; brings `mio`, `socket2`, `pin-project-lite`, `tokio-macros` | `async-std`/`smol` runtimes (quinn supports them less closely); hand-driving `quinn-proto` (a runtime of our own) |
| `socket2` | 0.6.5 | `jackstay-net` | The link's UDP socket with `IPV6_V6ONLY` off, so one socket serves IPv4 and IPv6 whatever `net.ipv6.bindv6only` says (quinn's server constructor leaves it to the sysctl); already in the graph through `quinn-udp` and `tokio` | `rustix` sockets (no `UdpSocket` conversion for quinn) |
| `signal-hook` | 0.4.5 | `jackstayd` | Turns `SIGTERM`/`SIGINT` into a byte on a pipe the loop polls, so stopping needs no `unsafe` of ours; no default features; brings `signal-hook-registry` and `libc` | `rustix` signals (installing a handler is `unsafe`); `ctrlc` (SIGINT only, a thread) |
| `gtk` | 0.19.0 | `jackstay-ui` | GTK 3, Xfce's toolkit, for the popup, tray menu and settings (plan §4.3, owner's choice of gtk3-rs). Feature `v3_24`. Brings the gtk-rs family at 0.22 (`glib`, `gio`, `gdk`, `gdk-pixbuf`, `cairo-rs`, `pango`, `atk`, their `-sys` crates and macros), `futures-*` through `glib`'s defaults, and at build time `system-deps` (with `toml`, `cfg-expr`, `target-lexicon`, `version-compare`, `heck`), `proc-macro-crate`, `syn` and `rustc_version`. `unicode-ident` and `target-lexicon` are allowed their licenses in `deny.toml`, build-time only. Single maintainer since its 2024 revival (plan §4.3): pinned, and the UI can be rewritten in C + GTK 3 behind the same socket without touching the daemon | C + GTK 3 (two languages, the protocol's types written twice); GTK 4 (its X11 backend is deprecated; Xfce is GTK 3); egui, iced, slint (foreign on Xfce, no tray) |
| `image` | 0.25.10 | `jackstay-thumbnail`; `jackstayd` (tests: images to copy) | Decodes clipboard images for thumbnails (docs/daemon.md, "Thumbnails"), in pure Rust, only in the confined helper. No default features (no `rayon`, no AVIF's C, no EXR, TIFF or the other formats); features `png`, `jpeg`, `webp`, `gif`, `bmp`, which bring `png` (with `fdeflate`, `flate2`, `miniz_oxide` in two versions, `crc32fast`, `adler2`, `simd-adler32`), `zune-jpeg` and `zune-core`, `image-webp`, `gif` with `weezl` and `color_quant`, `moxcms` and `pxfm` (colour management), `bytemuck`, `byteorder-lite`, `num-traits`, `quick-error`. Its decoders hold `unsafe` (SIMD, `bytemuck`): that is why they run only in a process that has confined itself first, and why the daemon trusts nothing back but a checked encoding | gdk-pixbuf (C, its loaders a long CVE history, and plan §4.3 rules it out); `zune-image` (younger, less used); a decoder per format by hand |
| `seccompiler` | 0.5.0 | `jackstay-thumbnail` | Builds and installs the thumbnail helper's seccomp filter (docs/daemon.md, "Thumbnails"): an allowlist of system calls with argument checks, so a decoder exploit cannot reach files, processes or sockets. rust-vmm's, used by Firecracker; no default features (no JSON frontend); brings only `libc`. Its `unsafe` (the `seccomp` call) stays in it, so ours stays forbidden | `libseccomp` (C, FFI); Landlock (filesystem only: signals and descriptors stay open, and needs a 6.x ABI for the rest); writing the BPF by hand (`unsafe` in our code) |
| `libc` | 0.2.191 | `jackstay-thumbnail` | System call numbers for the seccomp allowlist (`SYS_*`); already in the tree through `signal-hook` and `seccompiler`, at the same version | hard-coded numbers (wrong on another architecture, unreviewable) |
| `proptest` | 1.12.0 | tests only | Property tests for every decoder (TS-16): round trips, prefixes, arbitrary bytes | `quickcheck` (no shrinking strategies as composable); hand-rolled generators |

Spikes under `spikes/` have their own lockfiles and are not part of the product. Each fuzz crate
(`crates/*/fuzz`) has its own lockfile, and the gate builds each and checks it with cargo-deny.

## Tools in the image

Built from source with `cargo install --locked` at exact versions (Dockerfile):

| Tool | Version | Why |
|---|---|---|
| Rust | 1.99.1 (rustup 1.29.2, checksum-pinned) | toolchain; `rust-toolchain.toml` |
| cargo-deny | 0.20.2 | advisories, licenses, sources, bans |
| cargo-mutants | 27.1.0 | mutation testing of checkers and regression tests (TS-5) |
| cargo-fuzz | 0.13.2 | fuzzing every decoder, on nightly-2026-10-15 only |

The matrix's applications (docs/conformance.md, "Application tier"), at the owner's versions. They
are tools the tests run, never linked or shipped; the image that holds them is built locally and
in CI and not published.

| Application | Version | From | License |
|---|---|---|---|
| Firefox | 156.0 | Mozilla's release tarball, checksum-pinned (Ubuntu's `firefox` package is a snap stub) | MPL-2.0 |
| Chrome for Testing | 155.0.8059.39 | Google's Chrome for Testing build, checksum-pinned (Ubuntu's Chromium is a snap) | BSD-3-Clause and others (Chromium's) |
| VS Code | 1.137.0 | Microsoft's tarball, checksum-pinned: the Electron application the owner uses | Microsoft's product license (use, not redistribution) |
| LibreOffice Writer | 26.2.6.3 | the apt snapshot, with its GTK 3 plugin and `python3-uno` | MPL-2.0 |
| xfce4-terminal | 1.1.5 | the apt snapshot | GPL-2.0-or-later |
| xfsettingsd (xfce4-settings) | 4.20.1, with libxfce4ui 4.20.2 | the apt snapshot: its clipboard manager, which jackstayd cannot run beside (`tests/xfsettingsd.rs`) | GPL-2.0-or-later |
