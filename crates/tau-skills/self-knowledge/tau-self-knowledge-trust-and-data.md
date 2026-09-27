---
name: tau-self-knowledge-trust-and-data
description: Use before sharing private source or credentials with Tau; explains file authority, provider and integration egress, local retention, and safe support evidence.
advertise: false
---

# Decide what Tau can access and retain

Before the first model request, choose a bounded repository task and inspect
the active role's tools. Shell and filesystem extensions can read and modify
files visible to their configured processes when role policy allows; "do not
edit" in a prompt is not an enforced read-only workspace. Configured
extensions are trusted same-user executables. Mount restrictions are defense
in depth, not a malicious-code sandbox. Persistent supervised extensions
receive other Tau state recursively read-only by default; configure
`tau_state_access: hidden` to hide unrelated state where appropriate.

Request context sent to the selected inference provider can contain prompts,
source, images, tool results, and tool definitions. Network tools and enabled
integrations can contact their own configured services. Review that provider's
account terms and data controls; ChatGPT/Codex entitlement is not API-key
billing. Provider-hosted search and extension-hosted search have different
egress routes.

Default Linux roots are `${XDG_CONFIG_HOME:-$HOME/.config}/tau/` for config
and `${XDG_STATE_HOME:-$HOME/.local/state}/tau/` for credentials, sessions,
agents, logs, and artifacts. Durable session and agent cleanup is disabled
by default; diagnostics default to 30 days. Durable provider request/response
captures are default-on where supported, owner-private, and potentially
contain full prompts, paths, tool data, or account content. Captures are
best-effort: an absent file does not prove no provider call happened. Shared
artifact originals can outlive even ephemeral transcripts; `null` retention
disables cleanup rather than storage.

For support, start with version, install route, OS/kernel, failing step, and
redacted exact error. Inspect private logs and captures locally; do not send
the whole state tree, transcript, or capture by default. See
`docs/trust-and-data.md`, `SECURITY.md`, `tau-self-knowledge-isolation`,
`tau-self-knowledge-secrets`, and `tau-self-knowledge-artifacts`.
