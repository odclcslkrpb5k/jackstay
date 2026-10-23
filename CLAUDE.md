# Jackstay — agent instructions

- **Write only inside this repository.** Durable agent context goes in `docs/claude-notes/`
  (gitignored; start at its `INDEX.md`), not in any memory directory outside the repo.
- The plan is [`docs/plan.md`](docs/plan.md): written, red-teamed, committed.

## House standard

[Paranoid TypeScript](https://github.com/odclcslkrpb5k/paranoid-typescript), last read at
`ed089e4c2d1561fbe8eb8d7ade2c626394d13afe`. The stack is not chosen yet, so conformance is not
claimed. The standard is maintained by another agent session (`paranoid-typescript`); send it
proposals, never edit it.

### Deviations

- **RP-3** (one notes file): notes are a folder of topic files with an index, and gitignored. Reason:
  the owner's instruction (2026-10-23).
