# ARCH-tau-ext-pim: tau-ext-pim architecture

External ingress is constrained by [ARCH-external-message-boundary](../../../specs/ARCH-external-message-boundary.md).
For successful tool-result display metadata, emitted
`ToolResult.result.status` retains operation-specific outcomes, while
completed operations use `ToolUseState.status = Success` and
`ToolUseState.status_text = "ok"`. `approval_required` is the sole PIM
`status_text` exception because it documents a non-success lifecycle state: the
requested side effect remains pending rather than having completed.

`tau-ext-pim` is Tau's personal information management extension. It owns local
runtime policy, persistent extension state, and provider protocol glue for email
and calendar features while preserving Tau's model-visible output and secret
handling boundaries.

## Extension runtime shape

`src/main.rs` starts the extension process and `src/lib.rs` owns protocol
configuration, tool/action registration, and dispatch to feature runtimes. The
top-level PIM runtime uses `tau-client` for startup publication, exact
subscriptions, configuration error emission, live-only tool/action dispatch, and
writer lifecycle. The top-level `RuntimeState` keeps separate email and calendar
runtime states plus shared extension configuration.

Each configured instance projects its validated effective email and calendar
runtime state into its own action schema. Initial Configure-derived publication
is staged before `Ready`; a later successful Configure replaces that instance's
schema. A rejected Configure disconnects the extension and unregisters its
schema. The harness stamps schema owner identity, while structural tool prefixes
do not rewrite action roots or mix inventories between instances, following
[`SPEC-extension-tool-prefixes`](../../../specs/SPEC-extension-tool-prefixes.md).

Persistent extension-data storage goes through tau-client's manual-runtime
`ExtensionDataClient`. It generates request ids, waits for matching
`ExtensionDataResult` frames, and returns unrelated harness frames to the manual
loop instead of reading the protocol transport directly. PIM still owns storage
schema, feature policy, and conversion of harness storage errors into
email/calendar behavior.

The crate still contains an email-only runner path used by email-module tests and
local helpers. It now uses tau-client for protocol startup/dispatch like the
top-level PIM runtime, but it intentionally keeps using `FsStorage` rooted at
`Configure.state_dir` so email-module tests and local helpers do not depend on
harness extension-data storage.

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
tokens, and pending Google installed-app PKCE authorization records. Outgoing
approval denials are terminal tombstones under `approvals/outgoing/denied`;
runtime approval paths must treat a denied id as higher priority than stale
pending state if both records ever coexist after partial persistence failure.
Calendar state includes calendar approval queues, cached provider metadata such
as ETags where needed, Google OAuth refresh tokens, and pending Google
device-authorization records. Stored approval, OAuth, policy, and audit records validate schema, status, account, identifiers, and safe line fields before use. Audit logs remain sanitized metadata only and never persist message bodies, calendar descriptions, credentials, private URLs, ETags, raw authentication headers, or unbounded provider payloads.

Provider credentials, refresh tokens, access tokens, device codes, PKCE
verifiers, and private provider URLs are private extension state or harness
secrets. Pasted authorization-code redirect URLs are transient sensitive user
input because they contain one-time authorization codes. None of these values
may be written to model-visible tool/action output, prompt fragments,
self-knowledge, or diagnostics.

## Shared Google OAuth

`google_oauth.rs` is the shared Google OAuth 2.0 device-flow, installed-app
PKCE, and token-refresh implementation. Gmail email uses the installed-app
authorization-code flow with a Desktop OAuth client, a helper-owned fixed
`https://mail.google.com/` scope, and a manually pasted loopback redirect URL
because Google's device endpoint rejects the full Gmail IMAP/SMTP scope. Google
Calendar keeps the device flow with a TVs/Limited Input OAuth client and passes
its Calendar scope into the shared device-flow helper. Email and calendar
provide feature-specific account ids and state read/write callbacks while the
helper owns common request/response parsing, access-token caching, and secret
lookup.

Google OAuth configuration always names a harness secret for the OAuth client id
and may also name an optional client-secret secret. Refresh tokens have two
modes:

- Manual mode: config names a `refresh_token_secret`; auth actions are refused
  and the backend reads the refresh token from the harness secret map.
- State-owned mode: no `refresh_token_secret` is configured; `:email auth google`
  stores the refresh token in private extension state after installed-app PKCE
  authorization completes, while `:calendar auth google` stores it after the
  device flow completes.

Access tokens are cached in memory per account and refreshed on demand. When a
provider reports an authentication failure, callers may invalidate the cache and
retry only the authentication step. Retries must not repeat side-effecting work
that may already have been accepted by a provider. User-facing OAuth actions may show only the verification or authorization URL, user code, and paste instructions appropriate to the flow; device codes, authorization codes, PKCE verifiers, and tokens stay hidden. State-owned OAuth records validate account and schema on load, while accounts configured for secret-owned refresh tokens refuse state-owned authorization writes.

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
standard Tau header/payload list format documented in `AGENTS.md`. Email content remains hostile prompt input even when sender authentication passes. Raw `Authentication-Results` is not model-visible; trusted provider-added evidence may inform policy, but Tau does not itself verify DKIM. Backend errors are bounded and redact concrete token values.

## Calendar components

Calendar config and runtime code live under `calendar/`. `calendar/runtime.rs`
owns tool/action dispatch and approval policy. Provider backends are split by
source:

- `calendar/ics_feed.rs` reads configured ICS feed calendars.
- `calendar/google.rs` talks to Google Calendar APIs and uses the shared Google
  OAuth helper for device-flow and refresh-token handling.

Calendar ids exposed to models are opaque. Provider-specific identifiers,
approval state, and cached metadata stay behind the calendar runtime boundary. Writes keep provider concurrency tokens such as ETags internal and require approval by default. ICS feed URLs are private bearer-like URLs; non-loopback plaintext HTTP requires explicit configuration.

## Security and output policy

The crate treats provider responses and user-authored configuration as
untrusted. Config validation rejects unknown, deprecated, or inconsistent
provider/auth combinations instead of silently ignoring them.

All model-visible outputs must be bounded and sanitized so provider text cannot
create misleading columns, extra lines, terminal control effects, or secret
leakage. List outputs retain the standard header-then-payload shape and sanitize fields so untrusted text cannot forge rows or columns. Diagnostics that may contain provider error text must redact known
secrets and bearer tokens before they can reach logs or users.

OAuth behavior is governed by
[DECISION-tau-ext-pim-google-oauth-flow](DECISION-tau-ext-pim-google-oauth-flow.md)
and [SPEC-tau-ext-pim-google-oauth](SPEC-tau-ext-pim-google-oauth.md).
Testing guidance is in [`testing.md`](../testing.md).
