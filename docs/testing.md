# Testing guidelines

## Rendering themes

Rendering and theme behavior tests should use artificial fixture themes with
explicit semantic attributes. Do not snapshot or assert details of Tau's built-in
themes from renderer tests; built-ins are product defaults and may change for
readability without implying renderer behavior changed.

Built-in theme tests should be limited to parsing and intentional invariants of
those built-ins, such as the conservative default theme staying within its
allowed safe foreground colors and avoiding background colors.

## Terminal screen renderer boundaries

Terminal screen renderer tests should protect observable terminal behavior at
the boundaries where the in-memory screen model meets terminal scrolling. Prefer
focused `tau-term-screen` unit tests backed by `vt100::Parser` or the local
pending-wrap test model so assertions cover visible rows, scrollback order,
cursor position, exact-width pending-wrap transitions, shrink clearing, and
styled-cell output rather than only inspecting emitted escape bytes.

When refactoring renderer internals, keep the behavior-preserving contract
explicit: changed-range detection uses absolute content line indices, rows above
the previous viewport are treated as existing scrollback, missing new rows still
matter when old on-screen rows disappeared, and downward movement must continue
to scroll naturally at the bottom edge. Add regression tests for any newly found
terminal edge case instead of relaxing cargo-crap thresholds or accepting
snapshot-only coverage.

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


## Provider response streaming tests

Tests for `provider.response_updated` should use append-delta semantics: multi-update assistant/reasoning cases send only the newly appended suffix in each update. Do not feed full accumulated snapshots through delta helpers unless the test is explicitly checking legacy/invalid payload handling. Final-response tests should continue to assert complete `provider.response_finished.output_items`.

Progress metadata tests should assert byte-counter boundaries explicitly:
provider-side tests should prove which semantic output byte streams are counted
or excluded, and UI tests should prove progress remains transient and absent from
editor/final rendering.


## Provider stream repetition guard

When changing provider streaming parsers, add focused tests for assistant text, reasoning text, and tool-argument deltas. Tests should include high-volume exact loops that abort and negative cases for short repeated words, repeated prefixes with changing payloads, and line blocks below threshold.

Responses-style parsers must also cover final snapshot/done events (for example
`response.output_text.done`, tool argument/input done events, and
`response.output_item.done`) because providers can send complete content there
without earlier deltas.


## Skill discovery and loading

`tau-skills` tests should cover frontmatter parsing, validation helper
contracts, deterministic directory discovery, bounded discovery reads,
symlink-following for roots/directories/Markdown skill files, canonical-directory
cycle prevention, collision winner selection, scoped prompt defaults, and built-in
self-knowledge skills. Prefer focused fixtures that exercise one contract at a
time, including oversized bodies/frontmatter and UTF-8-safe truncation edge
cases.
