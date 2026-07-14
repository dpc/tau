# DESIGN-tau-cli-testing-strategy: tau-cli testing strategy

Status: unconfirmed

`dev_tmux` provider-access tests should stay focused on the security boundary:
config parsing, exact allowlist copying, stale scratch reconciliation, warning
behavior, and refusal of symlink, non-regular, path-traversal, or unsafe
source/destination entries.

Pure transcript renderers should be tested at the rendered block/span boundary,
not by snapshotting built-in theme implementation details. Rendering and theme
behavior tests must use representative fixture themes with distinct semantic
attributes, assert exact text preservation except for documented display-only
transforms such as table padding, and check that the resolved spans carry the
intended semantic styling. Built-in theme tests should only validate that the
embedded files parse and satisfy intentional theme-level invariants, so built-in
theme tweaks do not force unrelated renderer expectation churn.

Input-loop command routing tests should cover the emitted local notices and
harness events/prompts produced by routing decisions, not only tokenizer helper
functions. This is especially important for slash-command ownership boundaries
where CLI-owned commands, dynamic extension actions, harness-owned prompt
commands, and the unknown leading-slash fallback intentionally share similar
surface syntax.

Persistent prompt-history storage tests should cover the length-prefixed record
boundary: ordered round trips, bounded/unsupported/malformed records, torn or
oversized tails before append, and redaction/routing at the chat-command layer.
Keep these as focused unit tests around `prompt_history` plus routing tests for
command-line redaction; do not require interactive terminal E2E checks for
storage-format regressions.

Developer prompt and tool preview startup regressions must execute the bundled
`tau` binary against an isolated home and working directory. They should assert
observable rendered contributions from disabled-by-default extensions for both
public-environment-only and environment-plus-ordered-CLI cases; tests of parsed
override vectors or child `Command` environment fields alone do not cover the
spawned harness boundary.

Renderer transition tests should observe flush-delimited virtual-terminal frames
driven by the operation under test. Frame waits must be bounded and must not
request a post-operation `redraw_sync`, because that extra redraw can hide an
incoherent first frame. Race regressions may use a deterministic test-only
midpoint hook to request the premature redraw whose suppression is under test.
