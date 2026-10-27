# Dependencies

Every dependency is a review finding until it is justified here (TC-7). Versions are exact (`=`)
and `Cargo.lock` is committed; builds run `--locked`.

| Crate | Version | Used by | Why | Alternatives rejected |
|---|---|---|---|---|
| `libfuzzer-sys` | 0.4.13 | the fuzz crates only (`crates/*/fuzz`) | libFuzzer for cargo-fuzz (TS-16); its bundled libFuzzer is NCSA, allowed for this crate alone in `deny.toml` | `afl` (needs an instrumenting toolchain outside cargo); `honggfuzz` (less maintained bindings) |
| `x11rb` | 0.14.0 | `jackstay-x11-clip`; `jackstayd` (tests: who owns a selection) | The X protocol in pure Rust (`RustConnection`, no libxcb): the clipboard path has no C (plan §4.3). Features `xfixes` and `res` (X-Resource, for an owner's process): code generated into the same crate, no new ones | `xcb` crate (FFI to libxcb); `x11` (Xlib FFI) |
| `rustix` | 1.1.5 | `jackstay-x11-clip`, `jackstayd` | `poll` for the daemon's loop, `flock` for the instance lock, `SO_PEERCRED` on the control socket, `O_NOFOLLOW`, `getuid`; already in the tree through `x11rb`, at the same version | `libc` (unsafe FFI in our code); `mio` (more than a poll) |
| `sha2` | 0.11.1 | `jackstay-store` | Names each history blob by the SHA-256 of its bytes, so equal content is stored once and a blob can be checked against its name (docs/daemon.md, "Storage"). RustCrypto, pure Rust, no default features; brings `digest`, `block-buffer`, `crypto-common`, `hybrid-array`, `typenum`, `cpufeatures` (`cpuid` through `core::arch`, no libc on x86-64) and `cfg-if`, all RustCrypto's or long-standing | `ring` (C and assembly); a hand-written SHA-256 (unreviewed crypto); a non-cryptographic hash (a chosen collision would let one copy's blob stand for another's) |
| `toml` | 1.1.8 | `jackstayd` | Reads `config.toml` into a table; features `std`, `parse`, `serde` (for `serde_core`'s traits, no derive macros); brings `toml_parser`, `winnow`, `toml_datetime`, `serde_spanned`, `serde_core` | `serde` with derive (proc macros in the build for five keys); a hand-written parser (a second TOML dialect for users to learn) |
| `signal-hook` | 0.4.5 | `jackstayd` | Turns `SIGTERM`/`SIGINT` into a byte on a pipe the loop polls, so stopping needs no `unsafe` of ours; no default features; brings `signal-hook-registry` and `libc` | `rustix` signals (installing a handler is `unsafe`); `ctrlc` (SIGINT only, a thread) |
| `gtk` | 0.19.0 | `jackstay-ui` | GTK 3, Xfce's toolkit, for the popup, tray menu and settings (plan §4.3, owner's choice of gtk3-rs). Feature `v3_24`. Brings the gtk-rs family at 0.22 (`glib`, `gio`, `gdk`, `gdk-pixbuf`, `cairo-rs`, `pango`, `atk`, their `-sys` crates and macros), `futures-*` through `glib`'s defaults, and at build time `system-deps` (with `toml`, `cfg-expr`, `target-lexicon`, `version-compare`, `heck`), `proc-macro-crate`, `syn` and `rustc_version`. `unicode-ident` and `target-lexicon` are allowed their licenses in `deny.toml`, build-time only. Single maintainer since its 2024 revival (plan §4.3): pinned, and the UI can be rewritten in C + GTK 3 behind the same socket without touching the daemon | C + GTK 3 (two languages, the protocol's types written twice); GTK 4 (its X11 backend is deprecated; Xfce is GTK 3); egui, iced, slint (foreign on Xfce, no tray) |
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
