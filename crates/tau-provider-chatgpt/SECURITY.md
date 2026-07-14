# ChatGPT provider security boundary

Authenticated account quota is external-provider data governed by
[DESIGN-provider-quota-pacing](../../specs/DESIGN-provider-quota-pacing.md).
`/wham/usage` requests disable redirects, use a short timeout and body cap, and
never expose bearer/account headers or raw response bodies above this crate.
Pool ids use bounded normalized keys; oversized or colliding full snapshots
fail atomically. Missing/default/sole pools in account snapshots never prove
model applicability: only a supported in-band transport observation may carry
a route binding. The official WebSocket contract defines a valid nameless
`codex.rate_limits` turn event as the canonical default `codex` pool; a present
non-null malformed pool id in either optional field is rejected rather than
ignored or reinterpreted as that default. JSON null is treated as absence.
