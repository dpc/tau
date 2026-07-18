# DECISION-tau-cli-manual-tmux-e2e-boundary: Keep tmux E2E manual and scratch-isolated

Authority: confirmed, 2026-06-18, dpc

Agent-controlled terminal E2E checks use `tau dev tmux` as a manual boundary, not
a second automated framework. It launches a real Tau binary in a private tmux
server with scratch HOME/XDG state.

Real provider access is a narrow testing-only exception: an exact profile
allowlist may copy only the corresponding credentials into scratch state and must
fail closed on unsafe paths or file types. This accepts deliberate access to live
providers without copying the caller's other Tau config or state. The helper still
runs same-UID local processes with the user's permissions and is not a sandbox.

The exact helper contract is specified by
[`SPEC-tau-cli-dev-tmux`](SPEC-tau-cli-dev-tmux.md). Process ownership, scratch
cleanup, and trust boundaries are described by
[`ARCH-tau-cli`](ARCH-tau-cli.md).
