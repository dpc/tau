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
default, and enables only `core-shell` unless the tester explicitly does
something else after startup.

This workflow complements automated tests; it is not a replacement for focused
regression coverage. Reusable steps live in
`.agents/skills/tau-e2e-testing-tmux/SKILL.md`.
