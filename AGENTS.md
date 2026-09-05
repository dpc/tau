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
- Before designing a multi-stage event-log operation, read
  `specs/GATE-atomic-event-log-publication.md`.
- Treat logging and `tracing` sink I/O under
  `specs/ARCH-logging-io-analysis.md`; do not expand that exception
  to protocol, persistence, extension stderr draining, or other functional I/O.

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
- Before changing automatic-compaction policy scheduling, canonical terminal
  ownership/validation, `AgentPromptStarted.outer_turn_id`, outer-turn
  finishing, or retained completion retry, preserve focused initial- and
  post-tool-continuation oracles for both normal and canceled terminals. Assert
  the canonical terminal, outer-turn finish, and protected standalone start are
  each durable at most once; assert successful publication leaves no retained
  completion; and verify live append, cold replay, and the applicable restart
  cut. Keep manual, before-inference, reactive-overflow, and provider-shape
  coverage in their owning focused suites rather than multiplying a broad
  matrix.

- Before introducing a new free-form payload kind, or reframing an existing one, in the shared generic user-role `ContentPart::Text` carrier, read [`GATE-new-generic-user-payload-envelopes`](specs/GATE-new-generic-user-payload-envelopes.md) and [`SPEC-exact-sentinel-prompt-envelopes`](specs/SPEC-exact-sentinel-prompt-envelopes.md); use the shared registry rather than a component-local provenance wrapper. Typed tool results and the system/developer prompt channel are outside that rule.
