# DECISION-tau-e2e-deterministic-provider-boundary: Use a closed hermetic provider for acceptance

Authority: confirmed, 2026-07-16, dpc

Deterministic harness acceptance uses a test-only supervised provider with a
closed, versioned scenario grammar. It is launched by exact path, isolated from
ambient Tau startup transports, and never enters production packaging, discovery,
registries, or self-knowledge.

This boundary proves real harness integration and liveness while remaining
hermetic and fail-closed. It deliberately does not stand in for real backend wire
behavior, retry scheduling, crash-exact replay, or broad terminal rendering.

The exact grammar, isolation, checkpoint, and gate oracles are specified by
[`SPEC-tau-e2e-deterministic-provider`](SPEC-tau-e2e-deterministic-provider.md).
