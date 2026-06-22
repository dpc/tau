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
- Shared provider helpers, such as Google OAuth parsing and token caching, keep
  tests close to the helper so email and calendar behavior cannot drift.

Live Gmail/Google Calendar tests are intentionally not required in normal CI
because they would need user credentials and mutable external state. If manual
provider testing is done, do it outside automated tests with throwaway accounts
and never commit credentials, refresh tokens, access tokens, device codes, or
private URLs.
