Before changing this crate, discover and read the applicable Linked Specs in `specs/` and every wider `specs/` scope, then follow relevant links. Use the `linked-specs` skill when updating them and `linked-specs-review` when reviewing.

# ext-shell

File-mutation tools such as `edit` and `apply_patch` MUST attach structured UI-only diff payloads for changed UTF-8 files. The agent-visible tool result must stay minimal and must not include the diff.

Diff payloads MUST preserve unified-diff rendering data, including intra-line changed word/phrase segments for paired single-line replacements, so UIs can apply separate inline theme styles on top of added/removed line styles.

After major changes to this extension's features, tool behavior, UI payloads, configuration options, or user actions, update the built-in self-knowledge skill `tau-self-knowledge-ext-shell` so Tau can accurately explain the current extension behavior.

After changing exposed tool schemas, tool names, shell execution semantics,
directory-lock behavior, output formatting, truncation, UTF-8 handling,
backgrounding, cancellation, or wait semantics, update
`.agents/skills/tau-tool-verification` so future tool-verification runs check
the current behavior instead of stale assumptions.

Cwd metadata, remembered-cwd path resolution, and event sequencing rules are documented in `specs/ARCH-tau-ext-shell.md`; read and update it when touching those paths.

Protocol/UI/locking test coverage is documented in `testing.md`; read and update it when changing tool schemas, display state, directory-lock scheduling, or shell execution modes.

Security and reliability boundaries are documented in `specs/ARCH-tau-ext-shell.md`, `specs/SPEC-tau-ext-shell-directory-locking.md`, and `specs/SPEC-tau-ext-shell-process-lifecycle.md`; read and update them when changing shell execution, filesystem mutation, directory-lock behavior, process lifecycle, output draining, or cancellation.
