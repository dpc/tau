# DECISION-tau-provider-codex-replay-sidecars: Provider replay sidecars

Authority: unconfirmed

Responses replay preserves opaque provider syntax needed for cache and replay
continuity, while typed Tau fields remain semantic authority for text, phase, tool
routing, and pairing. Raw assistant sidecars are reused only after validating their
provider item kind.

This accepts bounded opaque provider data in durable replay to preserve fidelity
without allowing it to override Tau semantics. Exact validation and fallback are
specified by
[SPEC-tau-provider-codex-streaming-replay](SPEC-tau-provider-codex-streaming-replay.md).
