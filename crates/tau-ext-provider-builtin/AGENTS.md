Before changing this crate, discover and read the applicable Linked Specs in `specs/` and every wider `specs/` scope, then follow relevant links. Use the `linked-specs` skill when updating them and `linked-specs-review` when reviewing.

# tau-ext-provider-builtin

Read the applicable `specs/DESIGN-*.md` records and the applicable trust-boundary records under `specs/` before changing provider profile ownership/model publication, prompt worker/cancellation/retry behavior, diagnostics/persistence boundaries, event-driven worker wakeups, or this crate's testing boundary.

After major changes to this extension's features, tool/action behavior, configuration options, provider/runtime behavior, or user-visible capabilities, update the built-in self-knowledge skill `tau-self-knowledge-ext-provider-builtin` and user-facing provider docs (`docs/providers.md`, `FEATURES.md` as applicable) so Tau and the docs accurately explain the current extension behavior.
