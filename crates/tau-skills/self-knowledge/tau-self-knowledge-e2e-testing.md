---
name: tau-self-knowledge-e2e-testing
description: Use this skill when the user asks about Tau E2E testing, the dev tmux helper, scratch Tau state, testing.yaml, or opt-in provider access for testing agents.
advertise: false
---

# Tau E2E testing self-knowledge

Tau has a hidden `tau dev tmux` helper for manual, agent-controlled terminal E2E
checks of the current checkout. It is development infrastructure, not a
sandbox: commands still run as the local user.

## `tau dev tmux start`

`tau dev tmux start` creates a helper-owned scratch root with isolated:

- `HOME`
- `XDG_CONFIG_HOME`
- `XDG_STATE_HOME`
- `XDG_RUNTIME_DIR`

It starts Tau in a private tmux server, disables all extensions by default, and
enables `core-shell` with its working directory pointed at scratch `work/` (or
an explicit `--workdir`).

When no scratch path is supplied, `start` generates a unique temporary root and
prints it in the startup output. Use `--scratch-root` or its shorter `--root`
alias when a specific reusable scratch location is needed. The printed capture,
send, and stop commands include the selected scratch root. If target commands are
run without a root, they use the deterministic historical fallback root instead
of discovering the generated one.

## Provider access opt-in

Provider credentials are not copied by default. When `~/.config/tau/testing.yaml`
is absent, the helper prints a warning and the tmux Tau remains local-only.

To intentionally allow selected providers for manual E2E testing, create:

```yaml
testing_providers:
  - extension: provider-builtin
    provider: chatgpt
  - extension: provider-work
    provider: openrouter
```

Each entry is an exact configured extension instance and provider profile. To
discover profile names, run this in the real Tau environment before starting the
scratch tmux session:

```sh
tau provider list
```

The setup registration consists of credential-free settings and one typed
credential directory:

```text
~/.local/state/tau/providers/<extension>/<provider>.json
~/.local/state/tau/secrets/ext/<extension>/providers/<provider>/
```

For provider-management details, read
`tau-self-knowledge-ext-provider-builtin`.

Only those exact registration pairs are copied into scratch Tau state. Unrelated
providers, sessions, logs, general
`harness.yaml`, `cli.yaml`, and other user config/state are deliberately not
copied. `tau dev tmux start` enables every exact extension instance named by the
allowlist. It retains the canonical built-in identity for `provider-builtin` and
adds scratch-only built-in component and provider-role configuration for renamed
instances so their copied profiles can publish models.

If `testing.yaml` exists but `testing_providers` is empty, the helper warns and
copies no provider credentials.

## Security expectations

- Keep the default safe: no provider credentials are available unless
  `testing.yaml` explicitly lists exact providers.
- Do not ask Tau to copy all providers.
- Do not put path-like names in `testing_providers`; both names use validated Tau
  namespaces.
- Treat any copied provider credential as available to the tmux Tau process and
  the agent being tested.
