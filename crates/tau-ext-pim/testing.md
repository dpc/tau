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

Manual live Gmail or Calendar tests remain outside all automation and use
throwaway accounts.
Never commit credentials, refresh/access tokens, device codes, PKCE verifiers,
pasted redirects, or private URLs.
