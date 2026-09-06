---
name: tau-e2e-testing-deterministic
description: Run and diagnose Tau's always-on deterministic fake-provider end-to-end tests.
---

# Deterministic fake-provider E2E

Run the hermetic headless acceptance lane with:

```sh
cargo nextest run -p dpc-tau-e2e-tests --test deterministic_provider --no-tests=fail
```

Run the network-denied CI-equivalent lane with:

```sh
nix build -L .#ci.tests
```

The focused fake-provider command builds exact-path `tau-e2e-fake-provider`
and `tau-e2e-test-dummy` subprocesses. It needs no provider credentials,
network, VCR variables, shell, tmux, or sleeps. Do not set `TAU_VCR` for this
focused command.

Set `TAU_E2E_KEEP_ARTIFACTS=1` during local diagnosis to retain successful
private roots. Panic failures retain them automatically and print the path.
Artifacts include generated config/scenario, durable typed session events,
extension stderr, and the bounded semantic fake-provider trace.

This focused fake-provider command covers the harness/provider extension seam
and one deterministic dummy-tool continuation. It does not cover
provider-builtin, ChatGPT lowering/parsing, WebSocket behavior, production
retries, universal packaging, or terminal rendering. Keep VCR and transcript
replay separate.

`nix build -L .#ci.tests` builds and runs the complete network-denied CI test
derivation: workspace tests plus its deterministic post-checks. One separate
post-check pins the exact current-profile `tau-ext-provider-builtin` through
`TAU_E2E_PROVIDER_BUILTIN_BIN`; a direct unpinned workspace run intentionally
skips that test. See `docs/testing.md` for the direct pinned invocation. This
E2E covers a keyless loopback Chat Completions 429 park and ordinary manual
retry release, not automatic expiry or broader retry behavior.
