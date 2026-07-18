# SPEC-tau-cli-dev-tmux: Manual tmux E2E helper

The hidden `tau dev tmux` helper starts a real Tau binary in a private tmux server
and defaults to scratch HOME/XDG state. Its outer dispatch path does not load or
validate normal harness configuration; it rejects startup overrides that require
normal harness config resolution.

`start` generates a fresh temporary root when none is supplied and prints it before
fallible setup. `capture`, `send`, and `stop` retain the deterministic historical
fallback root when no root is supplied; generated-root workflows use the commands
printed by `start`.

Provider access reads only `testing.yaml` from the real Tau config directory.
Missing or empty configuration warns and keeps the child local-only. Non-empty
`testing_providers` values are exact provider-profile allowlist entries; there is
no all-providers mode. Only matching regular
`auth.d/<provider>.json` files may be copied. Provider lock files, general config,
sessions, logs, and unrelated profiles are never copied. Path traversal, symlinks,
non-regular files, and unsafe source or destination entries fail closed.
`provider-builtin` is enabled only while the allowlist is non-empty.

Implements
[`DECISION-tau-cli-manual-tmux-e2e-boundary`](DECISION-tau-cli-manual-tmux-e2e-boundary.md).
The trusted-local process and scratch-cleanup boundaries are described by
[`ARCH-tau-cli`](ARCH-tau-cli.md).
