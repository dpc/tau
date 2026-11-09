---
name: tau-self-knowledge-ext-pim
description: Use when configuring or operating Tau std-pim, including split email/calendar tools, Google Calendar OAuth, ICS calendars, PIM actions, approval workflow, audit logs, or PIM security policy.
advertise: false
---

# PIM: email and calendars

`std-pim` runs the separately installed `tau-ext-pim` executable. It provides
split email/calendar tools and `:email` / `:calendar` approval actions.

For Tau protocol 23.1, rebuild the extension with registry SDK crates
`dpc-tau-client`/`dpc-tau-proto` pinned to 0.29.0 and update it with the harness
before separately authorized activation.
Older-protocol binaries are rejected before configuration; changing the advertised
version alone is not a migration. The old `std-email` standard-instance alias
is removed; rename it to `std-pim`. PIM configuration accepts only the nested
`config.email` and `config.calendar` module keys.

Do not include private account or message/event data unless the user provided
it for that answer.


## Capabilities

- Email: list folders/messages, read, send, trash, star/unstar, mark read/unread,
  Gmail OAuth, and approval queues. Load `tau-self-knowledge-email` for setup,
  authentication, tools, and approval commands.
- Calendar: paginated event/free-busy reads, read-only timezone-aware ICS feeds
  with bounded recurrence expansion, and Google Calendar create/update/delete/RSVP.
  Writes require `:calendar change` approval by default.
- Both modules keep sanitized audit logs in extension state.


## Config shape

```yaml
{pim_config}
```

Migration and tool grants:

- Rename `extensions.std-email` to `extensions.std-pim`.
- Move old email fields from `extensions.std-email.config.*` to `extensions.std-pim.config.email.*`.
- Put calendar config under `extensions.std-pim.config.calendar.*`.
- Keep secret declarations under `extensions.std-pim.secrets`.
- Add the needed split tool names to any role's `enable_tools` list, for example `email_list_folders`, `email_read`, `email_send`, `calendar_list_calendars`, `calendar_search`, and `calendar_get`.


## Secrets

Declare names under `extensions.std-pim.secrets`. Secret files contain trimmed
UTF-8 text, not YAML:

```sh
mkdir -p ~/.local/state/tau/secrets
printf '%s\n' 'mail-password-or-app-token' > ~/.local/state/tau/secrets/mail_password.yaml
printf '%s\n' 'google-mail-desktop-oauth-client-id' > ~/.local/state/tau/secrets/google_mail_desktop_client_id.yaml
printf '%s\n' 'google-mail-desktop-oauth-client-secret' > ~/.local/state/tau/secrets/google_mail_desktop_client_secret.yaml
# Optional when using auth.refresh_token_secret instead of :email auth google:
printf '%s\n' 'google-mail-oauth-refresh-token' > ~/.local/state/tau/secrets/google_mail_refresh_token.yaml
printf '%s\n' 'https://example.com/private.ics' > ~/.local/state/tau/secrets/personal_calendar_ics_url.yaml
printf '%s\n' 'google-calendar-device-oauth-client-id' > ~/.local/state/tau/secrets/google_calendar_device_client_id.yaml
printf '%s\n' 'google-calendar-device-oauth-client-secret' > ~/.local/state/tau/secrets/google_calendar_device_client_secret.yaml
chmod 600 ~/.local/state/tau/secrets/*.yaml
```

Or for one startup, use `TAU_SECRET_<NAME>`, for example `TAU_SECRET_MAIL_PASSWORD=... tau`. Do not put passwords, refresh tokens, app passwords, OAuth tokens, or private ICS URLs directly in `harness.yaml`.


## Google email authorization

Gmail requires a `Desktop app` OAuth client, installed-app PKCE, and the
`https://mail.google.com/` scope. Do not reuse Calendar's device-flow client.
Use `:email auth google start <account>` followed by
`:email auth google finish <account> <copied-url>` with the full failed loopback
address-bar URL, or configure `auth.refresh_token_secret` for a manual token.
Load `tau-self-knowledge-email` for fields and troubleshooting.

Omit the account argument to list eligible Google accounts. Completion excludes
disabled, non-Google, and manual-token accounts. Eligible IDs contain at most
128 bytes of printable, non-whitespace ASCII.


## Google Calendar authorization

Omit `refresh_token_secret` to use device authorization. Create a Google OAuth
client of type `TVs and Limited Input devices`; other types fail with
`invalid_client: Invalid client type`. Configure `client_id_secret` and
`client_secret_secret`. The device endpoint requires the full Calendar scope;
it rejects some narrower scopes such as
`https://www.googleapis.com/auth/calendar.events`.

1. Start Tau with the `std-pim` config and the Google client id and client secret present.
2. Run `:calendar auth google start <account>`.
3. Open the printed URL manually and enter the printed user code.
4. Run `:calendar auth google finish <account>`.

The extension stores refresh tokens in private calendar state and shows only
the short-lived code and URL. Action events are transient by default.
To supply a manual token, declare/provide `refresh_token_secret`; interactive
authorization refuses to overwrite it. Restarting drops the in-memory access-token
cache, not the refresh token.


## Calendar policies and output safety

Recommended defaults:

- `calendar.policy.read.private_events: busy_only`
- `calendar.policy.read.descriptions: approved_only`
- `calendar.policy.write.require_approval: true`
- keep `calendar.accounts[*].calendars.allow` narrow

List results have headers, a blank line, then unindented space-separated rows.
`format` names the columns; the first column is the item ID (`uid` for email,
passed as `email_id`). Empty results contain `(no matches found)`. Defaults
appear in headers. Pass opaque folder/calendar IDs back unchanged.

`calendar_search` returns active events; use `include_cancelled: true` to
investigate cancellations. It omits Google tombstones without complete range
bounds. `calendar_get` can inspect a cancelled event by ID.
`calendar_free_busy` excludes cancelled, transparent, and self-declined events.
Both paginated reads default to 20 rows, allow at most 100, and return errors
rather than incomplete successful pages on safety failures.
For `next_cursor`, call the same tool with `{{ "cursor": "..." }}` only.
The cursor retains the query. Without `start`, reads begin at midnight two days
before today; without `end`, they end seven days after `start`.

ICS feed accounts should use `https://` or `webcal://` URLs, especially for private feed tokens. Non-loopback `http://` ICS feed URLs are rejected unless the account backend sets `allow_plain_http: true`; loopback HTTP remains available for local tests.

On `approval_required`, wait for user review via `:calendar change list`,
`:calendar change open <id>`, and `:calendar change approve <id>`.
Google writes use cached ETags. A rejection after dispatch, including an ETag
mismatch, has an unknown remote outcome: reconcile in Google Calendar, do not
retry. Approved unknown-outcome changes remain in `sending` and block reapproval.

## Email policies and output safety

Load `tau-self-knowledge-email` before configuring email policy or approvals.
Keep incoming authentication enabled and allowlists narrow. Never retry
`smtp_outcome_unknown` automatically: the provider may have accepted the email.


## Troubleshooting

If PIM tools are unavailable:

- Check `extensions.std-pim.enable: true`.
- Check module enables: `config.email.enable: true` and/or `config.calendar.enable: true`.
- Check the role enables the relevant split tools, for example `email_read` or `calendar_search`.
- Check startup config errors for missing required secrets.

If Google Calendar says the account is not authorized, run `:calendar auth google start <account>` and `:calendar auth google finish <account>`. If start returns `invalid_client: Invalid client type`, replace the client id with one from a Google OAuth client of type `TVs and Limited Input devices`. If it still fails, confirm the Google OAuth client is valid and has Calendar API access/scopes.

If ICS calendar reads fail, verify the private ICS URL secret is present and reachable from the Tau process.

For tracing logs, use:

```sh
TAU_LOG='pim=debug,warn' tau
```
