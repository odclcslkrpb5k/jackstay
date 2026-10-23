# Jackstay — agent instructions

- **Write only inside this repository.** Durable agent context goes in `docs/claude-notes/`
  (gitignored; start at its `INDEX.md`), not in any memory directory outside the repo.
- The plan is [`docs/plan.md`](docs/plan.md): written, red-teamed, committed.

## House standard

[Paranoid TypeScript](https://github.com/odclcslkrpb5k/paranoid-typescript), last read at
`ed089e4c2d1561fbe8eb8d7ade2c626394d13afe`. The stack is Rust; the standard has no Rust appendix
yet (one is being written from this project's measurements), so the core applies as written where
it can, mapped below. The standard is maintained by another agent session (`paranoid-typescript`); send it
proposals, never edit it.

### How the core maps to Rust here

- **TC-5** One gate: `./check` (re-runs itself in the container). CI runs exactly it.
  `tools/prove-checks` plants a bug per control and asserts the gate fails at that step.
- **TC-9** `./dev <cmd>` runs anything in the pinned container; a workstation needs only Docker.
- **TC-3** `Cargo.lock` committed, every check `--locked`, exact `=` versions.
- **TC-7** `docs/dependencies.md` justifies each crate.
- **CP-1, LN-1** `[workspace.lints]`: `unsafe_code = "forbid"`, clippy `pedantic`; `-D warnings`.
  A member gets them only with `[lints] workspace = true`, which the gate enforces.
- **ER-2, decoders** Crates that decode untrusted bytes deny `indexing_slicing`,
  `arithmetic_side_effects`, `unwrap_used`, `expect_used`, `panic`, `string_slice`, `unreachable`,
  `todo`, `unimplemented` at crate level, and read through a bounds-checked `Reader`.
- **TS-6** cargo-deny runs on the whole build graph (it skips dev-dependencies' licenses
  otherwise), and the gate asserts its graph equals cargo's.

### Deviations

- **RP-3** (one notes file): notes are a folder of topic files with an index, and gitignored. Reason:
  the owner's instruction (2026-10-23).
