Before changing this crate, discover and read the applicable Linked Specs in `specs/` and every wider `specs/` scope, then follow relevant links. Use the `linked-specs` skill when updating them and `linked-specs-review` when reviewing.

# tau-ext-std-notifications

After major changes to this extension's features, tool/action behavior, configuration options, provider/runtime behavior, or user-visible capabilities, update the built-in self-knowledge skill `tau-self-knowledge-ext-std-notifications` so Tau can accurately explain the current extension behavior.

Before changing event subscriptions, idle state tracking, hook configuration, or trigger semantics, read `specs/ARCH-tau-ext-std-notifications.md`.

Before changing terminal side effects, OSC user-var validation, command hooks, summary side-agent behavior, or template data flow, read the applicable trust-boundary records under `specs/`.

Notification configuration keys should use snake_case. Do not introduce kebab-case config keys or aliases unless the project intentionally changes that convention.
