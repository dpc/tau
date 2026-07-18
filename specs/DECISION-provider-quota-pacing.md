# DECISION-provider-quota-pacing: Correctness-first weekly quota pacing

Authority: confirmed, 2026-07-14, dpc

Tau presents provider account quota as bounded ephemeral current state. A
colored pacing claim requires fresh, explicit in-band evidence binding the
selected model to every applicable quota pool; account presence, display names,
or a sole reported pool are never applicability evidence. Missing, stale,
contradictory, or timing-untrusted inputs conservatively produce neutral unknown
rather than an inferred claim or fabricated rollover.

Provider adapters normalize upstream observations, the built-in provider owns
credentialed acquisition and reconciliation, the harness owns validated current
state, and the CLI owns generic accessible pacing presentation. Quota is not
transcript state, per-response usage, retry authority, credits, or spend control.

This correctness-first boundary avoids confident but false pacing guidance
across heterogeneous provider routes. Its cost is a neutral result until enough
fresh applicability and timing evidence exists.

Exact applicability, freshness, pacing, hysteresis, and presentation behavior is
specified by [SPEC-provider-quota-pacing](SPEC-provider-quota-pacing.md).
