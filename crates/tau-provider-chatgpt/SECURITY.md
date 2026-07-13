# ChatGPT provider security boundary

Authenticated account quota is external-provider data governed by
[DESIGN-provider-quota-pacing](../../specs/DESIGN-provider-quota-pacing.md).
`/wham/usage` requests disable redirects, use a short timeout and body cap, and
never expose bearer/account headers or raw response bodies above this crate.
Pool ids use bounded normalized keys; oversized or colliding full snapshots
fail atomically. Missing/default/sole pools never prove model applicability:
only an explicit supported transport observation may carry a route binding.
