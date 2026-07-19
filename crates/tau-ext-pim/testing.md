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
