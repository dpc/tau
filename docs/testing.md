# Testing guidelines

## Rendering themes

Rendering and theme behavior tests should use artificial fixture themes with
explicit semantic attributes. Do not snapshot or assert details of Tau's built-in
themes from renderer tests; built-ins are product defaults and may change for
readability without implying renderer behavior changed.

Built-in theme tests should be limited to parsing and intentional invariants of
those built-ins, such as the conservative default theme staying within its
allowed safe foreground colors and avoiding background colors.

## Manual Tau terminal E2E checks

Use `tau dev tmux` for agent-controlled manual checks of the real terminal UI
when behavior is too interactive for focused unit tests. The helper starts Tau in
a private tmux server with scratch `HOME`/XDG state, disables extensions by
default, and enables `core-shell`. It remains local-only unless
`~/.config/tau/testing.yaml` explicitly allowlists provider profile names under
`testing_providers`. When that file is absent or the list is empty, start prints
a warning and copies no real provider credentials/config/state.

Discover provider profile names in the real Tau environment with
`tau provider list`, then use the exact displayed profile name in
`testing_providers`. The name must match the stem of
`~/.local/state/tau/auth.d/<provider>.json`.

When providers are allowlisted, the helper copies only exact
`~/.local/state/tau/auth.d/<provider>.json` files into scratch state and enables
`provider-builtin` for the child Tau. It does not copy all providers, lock files,
general config, sessions, logs, or unrelated state.

This workflow complements automated tests; it is not a replacement for focused
regression coverage. Reusable steps live in
`.agents/skills/tau-e2e-testing-tmux/SKILL.md`.
