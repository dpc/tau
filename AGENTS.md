## Workspace

- `crates/` contains the major code components.
- This project uses the Linked Specs convention; consult the `linked-specs`
  skill before working with specs or governed code.
- `FEATURES.md` — major feature tour.
- `SECURITY.md` — reporting guidance and technical trust-boundary entry points.
- `docs/` — focused design and feature notes.
- `**/README.md` — component-specific human-oriented documentation where
  present.
- `**/AGENTS.md` — scoped agent instructions; read every applicable file before
  modifying code.
- Before raising security or denial-of-service requirements for IPC, identify the
  documented boundary in `SECURITY.md`: configured local extensions, cooperative
  inter-harness peers, and genuinely untrusted external ingress are distinct.
  Do not expand an unrelated feature into adversarial local-IPC hardening without
  an approved threat-model change.
- Before any architectural or externally meaningful functional change to event
  logs/journals or a harness-extension interface, read
  `specs/DECISION-persistence-and-extension-interface-change-approval.md`. Create
  a separately reviewed `DECISION-*` record for that change and obtain explicit
  user or maintainer confirmation before implementation; do not hide it in
  unrelated work.

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
