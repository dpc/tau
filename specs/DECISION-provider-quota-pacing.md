# DECISION-provider-quota-pacing: Correctness-first weekly quota pacing

Authority: confirmed, 2026-07-14, dpc

Tau presents provider account quota as bounded ephemeral current state. A
colored pacing claim requires fresh, explicit evidence binding the selected
model to every applicable pool. Missing, stale, contradictory, or timing-untrusted
inputs produce neutral unknown rather than inferred applicability or rollover.

Provider adapters normalize observations, the built-in provider owns credentialed
acquisition, the harness owns validated current state, and the CLI owns
presentation. Quota is not transcript state, retry authority, credits, or spend
control.

This avoids confident but false guidance at the cost of neutral output until
sufficient evidence exists. Exact behavior is specified by
[SPEC-provider-quota-pacing](SPEC-provider-quota-pacing.md).
