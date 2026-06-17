# ext-shell

File-mutation tools such as `edit` and `apply_patch` MUST attach structured UI-only diff payloads for changed UTF-8 files. The agent-visible tool result must stay minimal and must not include the diff.

Diff payloads MUST preserve unified-diff rendering data, including intra-line changed word/phrase segments for paired single-line replacements, so UIs can apply separate inline theme styles on top of added/removed line styles.

After major changes to this extension's features, tool behavior, UI payloads, configuration options, or user actions, update the built-in self-knowledge skill `tau-self-knowledge-ext-shell` so Tau can accurately explain the current extension behavior.

Cwd metadata, remembered-cwd path resolution, and event sequencing rules are documented in `ARCHITECTURE.md`; read and update it when touching those paths.

Protocol/UI/locking test strategy is documented in `design.md`; read and update it when changing tool schemas, display state, directory-lock scheduling, or shell execution modes.
