# DESIGN-tau-cli-tmux-e2e-helper: Manual tmux E2E helper

Status: confirmed, 2026-06-18, dpc

Manual terminal end-to-end checks should use the hidden `tau dev tmux` helper.
That helper is the accepted tmux-only boundary for agent-controlled manual Tau UI
testing: it starts a real Tau binary in a private tmux server, defaults to
scratch HOME/XDG state, and keeps the workflow manual rather than turning tmux
into a second automated test framework. The outer `tau dev tmux` dispatch path
must not load or validate the caller's normal harness configuration before
spawning the scratch child Tau; startup overrides that would require normal
harness config resolution are rejected at the outer helper boundary.

`tau dev tmux start` owns scratch-root generation: when no root is supplied, it
chooses a fresh temporary root and prints it before fallible scratch/provider
setup so failed starts remain easy to clean up. Target commands (`capture`,
`send`, and `stop`) keep the deterministic historical fallback root when no root
is supplied, but normal generated-root workflows should use the printed commands
from `start`.

Provider access in tmux E2E runs is an explicit testing-only exception to the
scratch-state default. `tau dev tmux start` may read only `testing.yaml` from the
real Tau config directory. Missing or empty testing config keeps the child
local-only and must warn. Non-empty `testing_providers` names are exact provider
profile allowlist entries; there is no "all providers" mode. The helper may copy
only corresponding real `auth.d/<provider>.json` files into scratch state, must
not copy provider lock files, general config, sessions, logs, or unrelated
profiles, and must fail closed on path traversal, symlink, non-regular file, or
unsafe destination conditions. `provider-builtin` is enabled in the child only
when the current allowlist is non-empty.
