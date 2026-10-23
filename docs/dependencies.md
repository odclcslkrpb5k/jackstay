# Dependencies

Every dependency is a review finding until it is justified here (TC-7). Versions are exact (`=`)
and `Cargo.lock` is committed; builds run `--locked`.

| Crate | Version | Used by | Why | Alternatives rejected |
|---|---|---|---|---|
| `libfuzzer-sys` | 0.4.13 | `jackstay-proto-fuzz` only | libFuzzer for cargo-fuzz (TS-16); its bundled libFuzzer is NCSA, allowed for this crate alone in `deny.toml` | `afl` (needs an instrumenting toolchain outside cargo); `honggfuzz` (less maintained bindings) |
| `proptest` | 1.12.0 | `jackstay-proto` tests only | Property tests for every decoder (TS-16): round trips, prefixes, arbitrary bytes | `quickcheck` (no shrinking strategies as composable); hand-rolled generators |

Spikes under `spikes/` have their own lockfiles and are not part of the product. The fuzz crate
(`crates/proto/fuzz`) has its own lockfile, and the gate builds it and checks it with cargo-deny.

## Tools in the image

Built from source with `cargo install --locked` at exact versions (Dockerfile):

| Tool | Version | Why |
|---|---|---|
| Rust | 1.99.1 (rustup 1.29.2, checksum-pinned) | toolchain; `rust-toolchain.toml` |
| cargo-deny | 0.20.2 | advisories, licenses, sources, bans |
| cargo-mutants | 27.1.0 | mutation testing of checkers and regression tests (TS-5) |
| cargo-fuzz | 0.13.2 | fuzzing every decoder, on nightly-2026-10-15 only |
