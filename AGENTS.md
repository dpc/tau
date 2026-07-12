## Workspace

- `crates/` contains the major code components.
- This project uses the Linked Specs convention; consult the `linked-specs`
  skill before working with specs or governed code.
- `FEATURES.md` — major feature tour.
- `docs/` — focused design and feature notes.
- `**/README.md` — component-specific human-oriented documentation where
  present.
- `**/AGENTS.md` — scoped agent instructions; read every applicable file before
  modifying code.

## Verification

- Use `cargo check --workspace --all-targets` to check Rust code.
- Use `cargo nextest run` for tests and `treefmt` for formatting.
- Before considering a change done, run final local CI with
  `selfci check --candidate <change-id>`.

## General guidance

- This project is still very immature; backward compatibility is not required.
- Always consult the `tau-commit` skill before making commits.
- When debugging existing Tau sessions, consult the
  `tau-self-knowledge-debugging` skill.
