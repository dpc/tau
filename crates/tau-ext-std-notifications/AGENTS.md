# tau-ext-std-notifications

After major changes to this extension's features, tool/action behavior, configuration options, provider/runtime behavior, or user-visible capabilities, update the built-in self-knowledge skill `tau-self-knowledge-ext-std-notifications` so Tau can accurately explain the current extension behavior.

Before changing event subscriptions, idle state tracking, hook configuration, or trigger semantics, read `ARCHITECTURE.md`.

Before changing terminal side effects, OSC user-var validation, command hooks, summary side-agent behavior, or template data flow, read `SECURITY.md`.

Notification configuration keys should use snake_case. Do not introduce kebab-case config keys or aliases unless the project intentionally changes that convention.
