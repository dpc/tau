Before changing this crate, discover and read the applicable Linked Specs in `specs/` and every wider `specs/` scope, then follow relevant links. Use the `linked-specs` skill when updating them and `linked-specs-review` when reviewing.

# tau-ext-pim

Read the applicable trust-boundary records under `specs/` before changing or reviewing PIM runtime behavior,
credential handling, persistent state, OAuth/provider integrations,
approval/policy logic, tool/action output, or backend network behavior.

Read `specs/ARCH-tau-ext-pim.md` before changing or reviewing provider/runtime wiring,
storage layout, shared OAuth helpers, or cross-module email/calendar boundaries.

Read [`testing.md`](testing.md) before changing or reviewing test strategy.

After major changes to this extension's features, tool/action behavior, configuration options, provider/runtime behavior, or user-visible capabilities, update the built-in self-knowledge skill `tau-self-knowledge-ext-pim` so Tau can accurately explain the current extension behavior. For email-specific configuration, policy, approval, or security changes, also update `tau-self-knowledge-email`.

Model-visible tool descriptions, schemas, prompt fragments, docs, and self-knowledge MUST treat email folder ids and calendar ids as opaque ids. Do not explain that they are flattened from account/folder or account/calendar internals, and do not expose `<account>/<folder>` or `<account>/<calendar>` in agent-facing text; say to use the ids returned by `email_list_folders` or `calendar_list_calendars`.

List-returning model-visible responses MUST follow Tau's standard tool-output shape: response headers first, one empty line, then an unindented line-oriented payload. Put `format: ...` in the headers to explain the space-separated payload columns, and put the main item key in the first payload column (for example message UID, event ID, account ID, or calendar ID). Put implicitly selected/defaulted values such as default account, calendar, `start`, or `end` in response headers, but do not repeat arguments that the model explicitly passed unless needed for clarity or disambiguation. Payload lines must be plain, unindented rows, not YAML-like lists or nested structures; empty lists MUST return a single `(no matches found)` payload line. Sanitize every line field so weird characters, whitespace, newlines, terminal/control characters, or untrusted content cannot break parsing or create misleading extra columns. Secondary structured arrays embedded in non-list detail responses, such as attachment metadata inside `email.read`, are not list-returning responses unless they are promoted to the top-level header/payload shape.
