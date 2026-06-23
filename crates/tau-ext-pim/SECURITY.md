# tau-ext-pim security notes

`tau-ext-pim` bridges Tau to personal email and calendar providers. Treat every
mailbox message, calendar event, provider error, folder/calendar name, MIME
header, and remote API response as untrusted external data.

## Credentials and OAuth tokens

- Passwords, OAuth client secrets, refresh tokens, access tokens, private ICS
  URLs, pending OAuth device codes, and PKCE verifiers are secrets stored in Tau
  secrets or private extension state. Pasted authorization-code redirect URLs
  are transient sensitive user input because they contain one-time authorization
  codes.
- Secrets must come from Tau extension secrets or private extension state; do
  not put token values in config examples, action output, model-visible tool
  output, audit logs, tracing spans, notices, or error messages.
- Google Calendar OAuth action output may show only the user-facing
  verification URL and user code. Google Gmail OAuth action output may show the
  installed-app authorization URL and instructions to paste the final loopback
  redirect URL into the finish action. It must never show the provider device
  code, pasted authorization code, PKCE verifier, refresh token, or access token.
- State-owned OAuth refresh tokens, pending device codes, and pending PKCE state
  must be stored under private extension state paths, with embedded
  account/schema validation on load. Accounts configured with
  `refresh_token_secret` must refuse state-owned `/email auth google` or
  `/calendar auth google` writes.
- Short-lived access tokens may be cached in memory, but cache errors must not
  reveal token values. Retry after auth failure must invalidate the cache before
  fetching a replacement token.

## Email provider boundary

- Email content is hostile prompt input even when sender authentication passes.
  Keep incoming body reads fail-closed behind policy or exact user approval.
- Raw `Authentication-Results` headers are not model-visible. The extension may
  use trusted provider-added evidence for policy decisions, but it does not
  cryptographically verify DKIM itself.
- Backend errors returned to model-visible tools or user actions must be
  bounded and sanitized. If an error path has access to a concrete token value,
  redact exact occurrences before formatting the error.
- IMAP/SMTP OAuth retries must be limited to authentication. Do not retry an
  entire SMTP message submission after `MAIL`, `RCPT`, or `DATA` may have run,
  because that can duplicate outgoing mail.

## Calendar provider boundary

- Calendar ids exposed to the model are opaque values returned by
  `calendar_list_calendars`; do not document internal encoding details in
  model-visible text.
- Calendar writes should keep provider concurrency tokens such as ETags
  internal and require user approval by default.
- ICS feed URLs are private bearer-like URLs. Prefer HTTPS/webcal, and allow
  non-loopback HTTP only when explicitly configured.

## Persistent state and logs

- Approval records, policy allowlists, OAuth records, and audit logs are
  extension-owned state. Validate schema, status, account, ids, and safe line
  fields on load before trusting records.
- Outgoing denied approval records are fail-closed tombstones. If a denied
  record and stale pending record coexist for the same id after a partial state
  update, user actions must treat the id as denied and refuse to approve or send
  it.
- Audit logs must stay metadata-only and sanitized. Do not persist email bodies,
  calendar descriptions, passwords, OAuth tokens, private URLs, provider ETags,
  raw auth headers, or unbounded provider payloads.
- Keep list-style model outputs in the standard header-then-payload shape and
  sanitize fields so untrusted text cannot forge extra rows or columns.
