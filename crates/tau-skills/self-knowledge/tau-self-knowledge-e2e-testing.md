---
name: tau-self-knowledge-e2e-testing
description: Use when running or explaining Tau end-to-end tests with dev tmux, scratch state, testing.yaml, or opt-in provider access.
advertise: false
---

# Tau E2E testing

The hidden `tau dev tmux` helper runs manual terminal E2E checks of the current
checkout. It is **not a sandbox**: commands still run as the local user.

## Scratch session

`tau dev tmux start` creates isolated `HOME`, `XDG_CONFIG_HOME`,
`XDG_STATE_HOME`, and `XDG_RUNTIME_DIR` under a unique temporary root. It starts
a private tmux server, disables extensions by default, and enables `core-shell`
in scratch `work/` (or `--workdir`).

Use the printed capture/send/stop commands, which include the generated root.
Commands without a root use the historical deterministic fallback, not discovery.
Supply `--scratch-root` (alias `--root`) for a reusable location.

## Provider access opt-in

No provider credentials are copied unless
`~/.config/tau/testing.yaml` lists exact extension-instance/provider-profile pairs:

```yaml
testing_providers:
  - extension: provider-builtin
    provider: chatgpt
  - extension: provider-work
    provider: openrouter
```

Discover names in the real environment before starting scratch:

```sh
tau provider list
```

An authenticated registration has credential-free settings and typed credentials:

```text
~/.local/state/tau/providers/<extension>/<provider>.json
~/.local/state/tau/secrets/ext/<extension>/providers/<provider>/
```

Keyless registrations have only settings. The helper copies only listed pairs
and enables their exact extension instances. It keeps `provider-builtin`'s
canonical identity and adds scratch-only built-in component/provider-role config
for renamed instances. It does not copy unrelated providers, sessions, logs,
`harness.yaml`, `cli.yaml`, or other user config/state.

Absent `testing.yaml` or an empty `testing_providers` list produces a warning
and copies no credentials. Do not request all providers or use path-like names;
both names use validated Tau namespaces. Treat copied credentials as available
to both the test process and its agent.

Read `tau-self-knowledge-ext-provider-builtin` for provider management.
