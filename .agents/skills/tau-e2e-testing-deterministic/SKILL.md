---
name: tau-e2e-testing-deterministic
description: Run and diagnose Tau's always-on deterministic fake-provider end-to-end tests.
---

# Deterministic fake-provider E2E

Run the hermetic headless acceptance lane with:

```sh
cargo nextest run -p tau-e2e-tests --test deterministic_provider --no-tests=fail
```

Run the network-denied CI-equivalent lane with:

```sh
nix build -L .#ci.deterministicE2eTests
```

The test package builds exact-path `tau-e2e-fake-provider` and
`tau-e2e-test-dummy` subprocesses. It needs no provider credentials, network,
VCR variables, shell, tmux, or sleeps. Do not set `TAU_VCR` for this lane.

Set `TAU_E2E_KEEP_ARTIFACTS=1` during local diagnosis to retain successful
private roots. Panic failures retain them automatically and print the path.
Artifacts include generated config/scenario, durable typed session events,
extension stderr, and the bounded semantic fake-provider trace.

This lane covers the harness/provider extension seam and one deterministic
dummy-tool continuation. It does not cover provider-builtin, ChatGPT
lowering/parsing, WebSocket behavior, production retries, universal packaging,
or terminal rendering. Keep VCR and transcript replay separate.
