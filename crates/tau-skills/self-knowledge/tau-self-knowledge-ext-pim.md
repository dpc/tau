---
name: tau-self-knowledge-ext-pim
description: Use this extension skill when the user asks how to configure Tau's std-pim extension, split email/calendar tools, Google Calendar OAuth, ICS calendars, PIM actions, approval workflow, audit logs, or PIM security policy.
advertise: false
---

# Tau std-pim extension self-knowledge

`std-pim` is Tau's built-in personal information management extension. It runs `tau-ext-pim`, registers model-visible split tools such as `email_list_folders`, `email_read`, `email_send`, `calendar_search`, and `calendar_create`, and publishes `/email` and `/calendar` user actions.

The legacy `std-email` built-in alias remains for old email-only configs. Prefer `std-pim` for new configs, especially when calendar support is needed. Do not enable both `std-pim` and `std-email` for the same account set.

Use this skill when helping a user configure PIM. Do not include personal addresses, server names, passwords, tokens, calendar URLs, event details, or message contents unless the user explicitly provided them for that answer.


## Capabilities

Email:

- list folders as opaque `<folder>` ids
- list recent messages
- read full mail only when policy or exact user approval allows it
- send, trash, star/unstar, mark read/unread
- queue unsafe incoming reads and outgoing sends for `/email` approval actions
- Gmail IMAP/SMTP Google OAuth2/XOAUTH2 via installed-app PKCE: `/email auth google start <account>` and `/email auth google finish <account> <copied-url>`
- append sanitized audit logs under the extension state directory

Calendar:

- list calendars as opaque ids
- list bounded event rows and free/busy rows with cursor pagination
- read one event by id
- read-only ICS feed accounts for standard `.ics` calendars, including timezone-aware event times and bounded recurrence expansion
- Google Calendar native API reads and user-approved writes: create, update, delete, and RSVP
- Google OAuth device authorization via `/calendar auth google start <account>` and `/calendar auth google finish <account>`
- append sanitized calendar audit logs and queue writes for `/calendar change` approval by default


## Config shape

`std-pim` nests module config under `config.email` and `config.calendar`.

```yaml
{pim_config}
```

Important migration from old email-only config:

- Rename `extensions.std-email` to `extensions.std-pim`.
- Move old email fields from `extensions.std-email.config.*` to `extensions.std-pim.config.email.*`.
- Put calendar config under `extensions.std-pim.config.calendar.*`.
- Keep secret declarations under `extensions.std-pim.secrets`.
- Add the needed split tool names to any role's `enable_tools` list, for example `email_list_folders`, `email_read`, `email_send`, `calendar_list_calendars`, `calendar_search`, and `calendar_get`.


## Secrets

Declare secret names under `extensions.std-pim.secrets`, then provide the values as Tau secrets. Secret files are trimmed UTF-8 text despite the `.yaml` suffix.

```sh
mkdir -p ~/.local/state/tau/secrets
printf '%s\n' 'mail-password-or-app-token' > ~/.local/state/tau/secrets/mail_password.yaml
printf '%s\n' 'google-mail-desktop-oauth-client-id' > ~/.local/state/tau/secrets/google_mail_desktop_client_id.yaml
printf '%s\n' 'google-mail-desktop-oauth-client-secret' > ~/.local/state/tau/secrets/google_mail_desktop_client_secret.yaml
# Optional when using auth.refresh_token_secret instead of /email auth google:
printf '%s\n' 'google-mail-oauth-refresh-token' > ~/.local/state/tau/secrets/google_mail_refresh_token.yaml
printf '%s\n' 'https://example.com/private.ics' > ~/.local/state/tau/secrets/personal_calendar_ics_url.yaml
printf '%s\n' 'google-calendar-device-oauth-client-id' > ~/.local/state/tau/secrets/google_calendar_device_client_id.yaml
printf '%s\n' 'google-calendar-device-oauth-client-secret' > ~/.local/state/tau/secrets/google_calendar_device_client_secret.yaml
chmod 600 ~/.local/state/tau/secrets/*.yaml
```

Or for one startup, use `TAU_SECRET_<NAME>`, for example `TAU_SECRET_MAIL_PASSWORD=... tau`. Do not put passwords, refresh tokens, app passwords, OAuth tokens, or private ICS URLs directly in `harness.yaml`.


## Google email authorization

For Gmail IMAP/SMTP, set `auth.method: oauth2`, `auth.provider: google`, `auth.client_id_secret`, and optionally `auth.client_secret_secret`. Omit `auth.refresh_token_secret` and authorize with `/email auth google start <account>` then `/email auth google finish <account> <copied-url>`, or set `refresh_token_secret` for a manually provisioned refresh token. Gmail requires the broad `https://mail.google.com/` scope for IMAP/SMTP XOAUTH2, and Google's device flow rejects that scope, so state-owned Gmail auth uses installed-app authorization-code + PKCE with manual loopback URL paste.

Google auth account arguments complete from the current enabled,
interactive-flow-eligible account inventory for that email or calendar module.
Omitting the account lists a bounded sorted inventory, or states that no
eligible accounts are available. Disabled, non-Google, and manual-refresh-token
accounts are excluded, as are account ids that cannot be inserted exactly as one
safe slash-command token. Eligible ids are at most 128 bytes and contain only
printable, non-whitespace ASCII.

Use a Google OAuth client of type `Desktop app` for Gmail; do not reuse the Calendar TVs/Limited Input device-flow client. Start prints a browser URL, the browser eventually fails to connect to `http://127.0.0.1:<port>/`, and finish expects the full copied address-bar URL. Google OAuth apps left in Testing mode may issue refresh tokens that expire after roughly 7 days for sensitive/restricted scopes; real use should use an Internal/trusted Workspace app or a properly published/verified app. Workspace administrators may still block untrusted OAuth apps. If `refresh_token_secret` is set, `/email auth google` refuses to overwrite it; remove that field first to use state-owned OAuth. These OAuth actions are not controlled by `email.policy.allow_state_policy_extensions`; that policy only controls whitelist actions. Access tokens are cached in memory until near expiry and are retried once after IMAP/SMTP authentication failure. Action output never includes pasted codes, PKCE verifiers, refresh tokens, or access tokens.


## Google Calendar authorization

For new Google Calendar configs, omit `refresh_token_secret` and use the device flow. Create a Google OAuth client of type `TVs and Limited Input devices`; desktop, web, Android, and iOS client IDs fail the device authorization start request with `invalid_client: Invalid client type`. Google may show both a client id and client secret for that client type. Configure both `client_id_secret` and `client_secret_secret`; the start request uses the client id, and the finish/token exchange can use the client secret. The device flow requests the full Google Calendar scope because Google's device endpoint rejects some narrower Calendar scopes such as `https://www.googleapis.com/auth/calendar.events`.

1. Start Tau with the `std-pim` config and the Google client id and client secret present.
2. Run `/calendar auth google start <account>`.
3. Open the printed URL manually and enter the printed user code.
4. Run `/calendar auth google finish <account>`.

The extension stores the returned refresh token in private calendar state. The action output never includes the refresh token. Action events are transient by default, and only the short-lived user code and URL are shown.

Manual refresh tokens still work for power users by setting `refresh_token_secret` on the Google backend and declaring/providing that secret. If `refresh_token_secret` is set, `/calendar auth google` refuses to overwrite it; remove the field first to use state-owned OAuth.

Google access tokens are short-lived and cached in memory until near expiry. Restarting Tau drops the access-token cache but keeps the refresh token.


## Calendar policies and output safety

Recommended defaults:

- `calendar.policy.read.private_events: busy_only`
- `calendar.policy.read.descriptions: approved_only`
- `calendar.policy.write.require_approval: true`
- keep `calendar.accounts[*].calendars.allow` narrow

PIM list-style tool results should follow Tau's standard header-then-payload shape: headers such as `format: ...` first, one empty line, then plain unindented rows. The `format` header describes the space-separated payload columns, and each item row starts with the main item id. Empty lists use a single `(no matches found)` payload line. Email folder ids and calendar ids are opaque ids returned by list tools; pass those opaque ids back exactly as returned by `email_list_folders` or `calendar_list_calendars`, without decoding or rewriting them. Email list rows still name the first column `uid`; pass that value as `email_id` to message-targeting tools such as `email_read`. Implicit/default values such as selected folder/calendar or defaulted calendar range bounds are response headers instead of repeated in every row. `calendar_search` and `calendar_free_busy` are bounded reads; if `start` is omitted they default to midnight 2 days before the current date, and if `end` is omitted it defaults to 7 days after `start`. Range read results include effective `start`/`end` headers; reuse those while paginating.

ICS feed accounts should use `https://` or `webcal://` URLs, especially for private feed tokens. Non-loopback `http://` ICS feed URLs are rejected unless the account backend sets `allow_plain_http: true`; loopback HTTP remains available for local tests.

Calendar writes should normally return `approval_required`; then the agent should wait for the user to inspect and approve with `/calendar change list`, `/calendar change open <id>`, and `/calendar change approve <id>`. Existing Google event writes use internally cached ETags; if the event changed, the agent should re-read it and retry.

## Email policies and output safety

Recommended defaults:

- keep `email.policy.incoming_auth.require: true`
- configure exact `email.policy.incoming_auth.trusted_authserv_ids`
- keep `email.policy.incoming_auth.allow_dmarc_only: false` unless the user explicitly accepts the weaker policy
- keep `incoming_allow`, `outgoing_allow`, and `folders.allow` narrow

Incoming email body reads are fail-closed. If policy does not allow full access, the model should use `email_request_access`, then wait for `/email in open <id>` and `/email in approve <id>`. Outgoing `email_send` calls that violate recipient policy queue under `/email out` actions; users can approve them with `/email out approve <id> [id...]` or reject them with `/email out deny <id> [id...]`.


## Troubleshooting

If PIM tools are unavailable:

- Check `extensions.std-pim.enable: true`.
- Check module enables: `config.email.enable: true` and/or `config.calendar.enable: true`.
- Check the role enables the relevant split tools, for example `email_read` or `calendar_search`.
- Check startup config errors for missing required secrets.

If Google Calendar says the account is not authorized, run `/calendar auth google start <account>` and `/calendar auth google finish <account>`. If start returns `invalid_client: Invalid client type`, replace the client id with one from a Google OAuth client of type `TVs and Limited Input devices`. If it still fails, confirm the Google OAuth client is valid and has Calendar API access/scopes.

If ICS calendar reads fail, verify the private ICS URL secret is present and reachable from the Tau process.

For tracing logs, use:

```sh
TAU_EXT_LOG=pim=debug tau
```
