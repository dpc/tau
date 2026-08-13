# PIM testing

Tests use deterministic local boundaries rather than live credentials. Cover
fail-closed config and secret references, deprecated/provider shapes, private
state paths and permissions, schema/account validation, deduplication and token
non-leakage, IMAP/SMTP/request formatting without accounts, fake-backend actions
and approval policy, sanitized logs, and public `run` startup, exact
subscriptions, dispatch boundaries, storage-before-config/backend selection,
`ConfigError` plus loop continuation, and live-only replay behavior. Shared
OAuth parsing and caching tests stay beside the helper so Gmail and Calendar
cannot drift.

Action-schema tests cover sorted effective email/calendar Google auth
inventories, explicit empty behavior, shared-schema bounds, safe-token filtering,
and omitted-account diagnostics without secrets or native OAuth state. Public
runner tests exercise Configure-derived initial schema publication and successful
same-instance replacement for both combined PIM and legacy email-only wiring,
including structural tool prefixes that leave action roots unchanged.

Manual live Gmail or Calendar tests remain outside all automation and use
throwaway accounts.
Never commit credentials, refresh/access tokens, device codes, PKCE verifiers,
pasted redirects, or private URLs.

Calendar query lifecycle tests follow
[`SPEC-calendar-query-lifecycle`](specs/SPEC-calendar-query-lifecycle.md) across
the distributed boundary: provider tests cover exact Google query serialization
and ICS visibility before feed-page slicing; runtime tests cover defensive
lifecycle and blocking filters, semantic page filling, provider budgets, and
cursor reconstruction; tool tests cover schemas; model-visible output tests
cover headers and row shape. Keep these layers deterministic and use loopback
feeds or scripted provider pages rather than live accounts.

Google Calendar mutation tests use deterministic loopback HTTP servers through
the real runtime and backend. They cover the pre-dispatch/unknown-outcome cut,
all mutation methods, response body/status/parser failures, complete success,
durable `sending` restart and same-ID refusal, direct-write residual state, and
bounded sanitized diagnostics. Do not replace these cross-layer oracles with
state-only tests or timing sleeps.
