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
`testing_providers` values are exact extension/provider allowlist pairs; there is
no all-providers mode. Only matching regular credential-free settings files and
typed credential subtrees may be copied. General config, sessions, logs, and
unrelated profiles are never copied. Path traversal, symlinks, non-regular files,
and unsafe source or destination entries fail closed.
The scratch Tau enables every extension instance named by the allowlist. The
canonical `provider-builtin` instance inherits its built-in component identity;
renamed instances receive exact scratch-only built-in component suffix and
provider-role configuration.

The trusted-local process and scratch-cleanup boundaries are described by
[`ARCH-tau-cli`](ARCH-tau-cli.md).
Provider access allowlists exact `(extension instance, provider)` pairs and
copies only those settings files and secret subtrees into private scratch state,
as required by
[SPEC-extension-secret-storage](../../../specs/SPEC-extension-secret-storage.md).
