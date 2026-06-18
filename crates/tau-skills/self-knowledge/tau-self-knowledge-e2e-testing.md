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

## Provider access opt-in

Provider credentials are not copied by default. When `~/.config/tau/testing.yaml`
is absent, the helper prints a warning and the tmux Tau remains local-only.

To intentionally allow selected provider profiles for manual E2E testing, create:

```yaml
testing_providers:
  - chatgpt
  - openrouter
```

Each entry is an exact configured provider profile name. To discover profile
names, run this in the real Tau environment before starting the scratch tmux
session:

```sh
tau provider list
```

Use the exact profile name shown there, which is also the stem of the real Tau
provider auth/profile file:

```text
~/.local/state/tau/auth.d/<provider>.json
```

For provider-management details, read
`tau-self-knowledge-ext-provider-builtin`.

Only those exact `auth.d/<provider>.json` files are copied into the scratch Tau
state. Lock files, unrelated provider profiles, sessions, logs, general
`harness.yaml`, `cli.yaml`, and other user config/state are deliberately not
copied. When the allowlist is non-empty, `tau dev tmux start` also enables the
`provider-builtin` extension inside the tmux Tau so copied profiles can publish
models.

If `testing.yaml` exists but `testing_providers` is empty, the helper warns and
copies no provider credentials.

## Security expectations

- Keep the default safe: no provider credentials are available unless
  `testing.yaml` explicitly lists exact provider profiles.
- Do not ask Tau to copy all providers.
- Do not put path-like names in `testing_providers`; provider names must be
  filename-safe Tau provider namespaces.
- Treat any copied provider credential as available to the tmux Tau process and
  the agent being tested.
