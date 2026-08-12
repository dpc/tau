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
  `specs/GATE-persistence-and-extension-interface-change-approval.md`. Obtain
  explicit user or maintainer confirmation of the exact semantics before
  implementation; do not hide the choice in unrelated work or create another
  gate unless the user explicitly requests one.

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


## Compaction prevention checklist

- Before changing `ContextItem`, standalone terminal handling, `AgentCompacted`,
  compact-request lowering, compact-response parsing, or opaque raw replay,
  consult `specs/SPEC-compaction-and-context-recovery.md`. For Codex request
  lowering, response parsing, or opaque replay, also consult
  `crates/tau-provider-codex/specs/ARCH-tau-provider-codex.md`.
- Update or run the named oracle coverage that applies: proto
  `compaction_window_accepts_provider_item_and_rejects_harness_trigger`,
  harness `standalone_rejections_do_not_mutate_context_or_compaction_authority`,
  core `standalone_compaction_opaque_windows_match_live_append_and_cold_replay`,
  and deterministic E2E
  `deterministic_opaque_standalone_compaction_replays_after_clean_restart`.
- Compact request lowering must also update or verify the exact
  `responses-compact-standard.json` and `responses-compact-lite.json` Codex
  fixtures. Compact-response parsing and opaque raw replay must verify
  `responses-compact-output.json`. Do not replace these focused rejection and
  live/replay oracles with broad matrix coverage.
