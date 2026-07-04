# Design decisions

This file records major design decisions currently embodied by this directory's
code, and how authoritative each decision is. It is not an architecture overview,
ADR log, todo list, roadmap, implementation guide, or changelog.

## Testing strategy

Status: inferred

The PIM extension has high-risk boundaries: untrusted provider data, private
credential state, user approval state, and model-visible tool/action output.
Tests should therefore prefer small deterministic unit tests around each
boundary over live provider integration tests.

- Configuration tests cover fail-closed defaults, explicit config errors,
  secret-reference validation, deprecated-field rejection, and provider-scoped
  auth shapes.
- State tests cover private file creation, safe path construction, schema and
  embedded-account validation, deduplication, and token/output non-leakage.
- Backend-format tests cover IMAP/SMTP protocol payloads and provider request
  selection without requiring live accounts.
- Runtime/action tests cover user-visible command behavior, approval queues,
  no-token output, sanitized logs, and policy gates using fake backends.
- Public `run` protocol tests cover startup ordering, exact subscriptions,
  `ConfigError` emission and loop continuation, live-only replay filtering,
  dispatch boundaries, and storage backend selection for migrated tau-client
  runtimes.
- Shared provider helpers, such as Google OAuth parsing and token caching, keep
  tests close to the helper so email and calendar behavior cannot drift.

Live Gmail/Google Calendar tests are intentionally not required in normal CI
because they would need user credentials and mutable external state. If manual
provider testing is done, do it outside automated tests with throwaway accounts
and never commit credentials, refresh tokens, access tokens, device codes, PKCE
verifiers, pasted redirect URLs, or private URLs.

## Google OAuth flow split

Status: confirmed, 2026-06-22, dpc

Gmail IMAP/SMTP OAuth and Google Calendar OAuth intentionally use different
Google OAuth client types and flow families. Gmail requires the restricted
`https://mail.google.com/` scope for IMAP/SMTP XOAUTH2, and Google's device-flow
endpoint rejects that scope, so state-owned Gmail auth uses a Desktop
installed-app authorization-code flow with PKCE and a manually pasted failed
loopback redirect URL. Google Calendar remains on the device flow with a
`TVs and Limited Input devices` client because that UX works for the Calendar
scope and avoids changing working Calendar behavior. Shared helper code may be
centralized, but email and calendar pending auth state stay separate so one
flow's migration or validation rules cannot silently weaken the other.
