# tau-ext-pim architecture

`tau-ext-pim` is Tau's personal information management extension. It owns local
runtime policy, persistent extension state, and provider protocol glue for email
and calendar features while preserving Tau's model-visible output and secret
handling boundaries.

## Extension runtime shape

`src/main.rs` starts the extension process and `src/lib.rs` owns protocol
configuration, tool/action registration, and dispatch to feature runtimes. The
top-level `RuntimeState` keeps separate email and calendar runtime states plus
shared extension configuration.

Each feature validates its raw config before becoming operational:

- `email::ValidatedConfig` and `calendar::config::ValidatedConfig` contain the
  fail-closed, provider-specific shapes used by runtime code.
- Invalid configuration is reported as an explicit extension configuration error;
  runtimes that cannot be configured remain inert rather than falling back to a
  weaker or implicit behavior.
- Runtime engines own their backend and state store references. Tests commonly
  instantiate engines with fake backends so tool/action behavior can be covered
  without live provider accounts.

## Persistent state and storage boundaries

`storage.rs` wraps harness extension storage paths. Feature modules build their
own typed state helpers on top of it and must keep path components safe and
account-scoped.

Email state includes approval queues, message-send logs, Google OAuth refresh
tokens, and pending Google device-authorization records. Calendar state includes
calendar approval queues, cached provider metadata such as ETags where needed,
Google OAuth refresh tokens, and pending Google device-authorization records.

Provider credentials, refresh tokens, access tokens, device codes, and private
provider URLs are private extension state or harness secrets. They must not be
written to model-visible tool/action output, prompt fragments, self-knowledge, or
diagnostics.

## Shared Google OAuth

`google_oauth.rs` is the shared Google OAuth 2.0 device-flow and token-refresh
implementation for Gmail and Google Calendar. Email and calendar provide
feature-specific account ids, OAuth scopes, and state read/write callbacks while
the helper owns common request/response parsing, access-token caching, and
secret lookup.

Google OAuth configuration always names a harness secret for the OAuth client id
and may also name an optional client-secret secret. Refresh tokens have two
modes:

- Manual mode: config names a `refresh_token_secret`; auth actions are refused
  and the backend reads the refresh token from the harness secret map.
- State-owned mode: no `refresh_token_secret` is configured; `/email auth google`
  or `/calendar auth google` actions store the refresh token in private extension
  state after the device flow completes.

Access tokens are cached in memory per account and refreshed on demand. When a
provider reports an authentication failure, callers may invalidate the cache and
retry only the authentication step. Retries must not repeat side-effecting work
that may already have been accepted by a provider.

## Email components

`email/mod.rs` owns email config validation, action/tool behavior, account and
folder id presentation, approval policy, send-log state, and the `EmailBackend`
trait used by tests and real provider code.

`email/real_backend.rs` is the network backend for configured IMAP and SMTP
accounts. Password accounts use the configured password secret. Gmail OAuth
accounts use Google access tokens:

- IMAP authenticates with `AUTHENTICATE XOAUTH2` payloads for the configured
  login identity.
- SMTP authenticates with `lettre`'s XOAUTH2 mechanism before message data is
  submitted.
- SMTP OAuth retry is bounded to pre-submission authentication. Message
  submission itself is not retried after provider SMTP errors, so Tau does not
  risk duplicate outgoing email after a server has accepted a message.

Folder ids and message ids exposed to models are opaque. Tool outputs use the
standard Tau header/payload list format documented in `AGENTS.md`.

## Calendar components

Calendar config and runtime code live under `calendar/`. `calendar/runtime.rs`
owns tool/action dispatch and approval policy. Provider backends are split by
source:

- `calendar/ics_feed.rs` reads configured ICS feed calendars.
- `calendar/google.rs` talks to Google Calendar APIs and uses the shared Google
  OAuth helper for device-flow and refresh-token handling.

Calendar ids exposed to models are opaque. Provider-specific identifiers,
approval state, and cached metadata stay behind the calendar runtime boundary.

## Security and output policy

The crate treats provider responses and user-authored configuration as
untrusted. Config validation rejects unknown, deprecated, or inconsistent
provider/auth combinations instead of silently ignoring them.

All model-visible outputs must be bounded and sanitized so provider text cannot
create misleading columns, extra lines, terminal control effects, or secret
leakage. Diagnostics that may contain provider error text must redact known
secrets and bearer tokens before they can reach logs or users.

See `SECURITY.md` for the security policy and `design.md` for design decisions
that future changes should preserve.
