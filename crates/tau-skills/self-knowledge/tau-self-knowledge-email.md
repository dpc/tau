---
name: tau-self-knowledge-email
description: Use when configuring or troubleshooting Tau std-pim email, including mail accounts, IMAP/SMTP, email approvals, incoming authentication, DKIM, Authentication-Results, or email security policy.
advertise: false
---

# Tau std-pim email configuration

Tau's standard PIM extension is named `std-pim`. It runs the separately
installed `tau-ext-pim` executable, registers split model-visible email tools such
as `email_list_folders`, `email_read`, and `email_send`, and publishes `:email`
approval/denial actions. The old `std-email` instance alias is removed; rename it
to `std-pim` and nest email settings under `config.email`.

Do not include private account data or message contents unless the user provided
them for that answer.


## Secure baseline

Enable the extension, email module, and each account; modules/accounts default
to disabled. Keep `policy.incoming_auth.require: true`,
`policy.incoming_auth.allow_dmarc_only: false`, and narrow `incoming_allow`,
`outgoing_allow`, and `folders.allow`. Set exact `trusted_authserv_ids` for the
trusted mailbox provider or final MTA. Approve edge cases manually rather than
disabling authentication. For config-only allowlists, set
`policy.allow_state_policy_extensions: false`.


## Example harness config

Put this in `~/.config/tau/harness.yaml` or a file under `~/.config/tau/harness.d/`, then replace all example values with the user's provider details.

```yaml
extensions:
  std-pim:
    enable: true
    secrets:
      mail_password: {}
      google_mail_desktop_client_id:
        optional: true
      google_mail_desktop_client_secret:
        optional: true
      google_mail_refresh_token:
        optional: true
    config:
      email:
        enable: true
        accounts:
          - id: work
            enable: true
            display_name: Work mail
            from: Alice Example <alice@example.com>
            imap:
              host: imap.example.com
              port: 993
              tls: required
              login: alice@example.com
            smtp:
              host: smtp.example.com
              port: 587
              tls: start_tls
              login: alice@example.com
            auth:
              method: password
              password_secret: mail_password
              # Gmail OAuth alternative:
              # method: oauth2
              # provider: google
              # client_id_secret: google_mail_desktop_client_id
              # client_secret_secret: google_mail_desktop_client_secret
              # Use a Google OAuth client of type "Desktop app".
              # Omit refresh_token_secret, run start, open the URL, then paste
              # the full failed loopback address-bar URL into finish:
              #   :email auth google start work
              #   :email auth google finish work http://127.0.0.1:54321/?state=...&code=...
              # Or set refresh_token_secret for a manually provisioned token:
              # refresh_token_secret: google_mail_refresh_token
            folders:
              allow:
                - INBOX
        policy:
          incoming_allow:
            - alice@example.com
            - '*@trusted.example'
          incoming_auth:
            require: true
            trusted_authserv_ids:
              - mx.example.com
            allow_dmarc_only: false
          outgoing_allow:
            - alice@example.com
            - '*@trusted.example'
          allow_state_policy_extensions: true
```

IMAP defaults to port 993 / `tls: required`; SMTP to 587 / `tls: start_tls`.
Password auth needs `auth.password_secret` and a matching secret declaration.
`auth.method: none` supports SMTP-only/relay setups, not IMAP.
OAuth supports Gmail, not generic providers; command-based password/token
sources are rejected.

List results contain headers such as `format: ...`, a blank line, then
unindented rows. Pass the leading message UID as `email_id` and folder IDs
unchanged. `email_read` attachment metadata is structured detail, not a list.


## Secrets

Declare the secret name in config:

```yaml
extensions:
  std-pim:
    secrets:
      mail_password: {}
```

Then set the value either as a state secret file or as a one-shot environment variable. The secret file is read as trimmed UTF-8 text even though the suffix is `.yaml`.

```sh
mkdir -p ~/.local/state/tau/secrets
printf '%s\n' 'app-password-or-token' > ~/.local/state/tau/secrets/mail_password.yaml
chmod 600 ~/.local/state/tau/secrets/mail_password.yaml
```

Or for one startup:

```sh
TAU_SECRET_MAIL_PASSWORD='app-password-or-token' tau
```

Tau normalizes the environment suffix to `mail_password`. Do not put passwords directly in `harness.yaml`.


## Incoming authentication and DKIM

The visible `From` is spoofable. Default auto-read requires all three:

1. `From` matches `policy.incoming_allow` or a persisted incoming whitelist pattern.
2. The newest parsed `Authentication-Results` header has an authserv-id listed in `policy.incoming_auth.trusted_authserv_ids`.
3. That trusted header reports `dkim=pass` with `header.d` aligned to the visible `From` domain.

The extension trusts the provider/MTA's `Authentication-Results`; it does not
verify DKIM itself. Copy the exact authserv-id from that trusted server's raw
header, here `mx.example.com`:

```text
Authentication-Results: mx.example.com; dkim=pass header.d=trusted.example; dmarc=pass header.from=trusted.example
```

Do not configure the sender domain as `trusted_authserv_ids` unless that is actually the authserv-id emitted by the trusted server.

Failures such as `auth missing`, `untrusted auth server`, `auth failed`,
`auth unaligned`, `dkim missing`, or `auth truncated` require manual approval.


## Allowlists and patterns

Incoming and outgoing allowlists accept:

- exact addresses: `alice@example.com`
- globs: `*@example.com`
- regexes with a `re:` prefix, matched against the whole normalized address: `re:.*@trusted\.example`

Incoming allow controls read exposure; outgoing allow controls delivery without
approval. Prefer exact addresses or narrow domains.

- `reply_to` is also checked for outgoing sends.
- If `email_send` returns `smtp_outcome_unknown`, SMTP may already have accepted
  the email. Do not retry automatically; reconcile with the account or provider
  before sending again. Tau does not internally resend message data.
- Bcc is checked for policy and visible to the user while a draft is pending via `:email out open <id>`, but hidden from model-facing send status and denied status output.


## Approval workflow

Incoming reads:

- `email_list_folders` returns visible folders and their opaque IDs.
- `email_list_recent(days=N)` uses the inclusive rolling `N × 24 hour` window
  ending at operation start, filtered by IMAP internal timestamp before limiting.
  Omit `folder` for the default. Rows redact untrusted details and include
  `access=full|preview|none`.
- `email_read` returns a full body only when policy or exact approval grants
  `full`. Simplified bodies use
  `<external_unstrusted_message>...</external_unstrusted_message>`.
- With `preview`, it returns `body_preview` without asking for approval: no
  HTML, links replaced by `LINK`, and only ASCII letters/digits, spaces, commas,
  and periods inside the wrapper.
- Use `email_request_access` for a preview/none message only when the preview or metadata justifies asking the user. Then use `:email in list`, `:email in open <id>`, and `:email in approve <id> [id...]`.
- Use `:email in deny <id> [id...]` to persist exact denials. Future matching reads report `access=none`; explicit `email_request_access` calls can ask again.
- After approval, the agent must repeat the matching `email_read` call.

Message management:

- The `email_mark_read`, `email_mark_unread`, `email_star`, `email_unstar`, and `email_trash` tools operate on a `folder` plus `email_id` target.
- These commands do not require content approval.
- `star` and `unstar` map to the IMAP `\\Flagged` flag.
- `email_trash` moves the message to Trash.

Outgoing sends:

- `email_send` sends immediately only when every `to`, `cc`, `bcc`, and `reply_to` address is allowed.
- Otherwise it queues the full draft and returns `approval_required`.
- Use `:email out list`, `:email out open <id>`, `:email out approve <id> [id...]`, and `:email out deny <id> [id...]` to review, send, or reject.
- Denied outgoing approval ids leave the pending queue and cannot later be approved or sent.
- If `email_send` returns `approval_required`, the agent should not call `email_send` again for the same draft.

Audit log:

Message listing, reading, access requests, sending, and management append
sanitized JSONL in email state.
`:email log last [number]` shows recent activity (default 20), without bodies.

Whitelist actions:

- `:email in whitelist <pattern>` persists an incoming pattern.
- `:email out whitelist <pattern>` persists an outgoing pattern.
- These actions only affect policy when `policy.allow_state_policy_extensions` is true.

Google OAuth actions:

- `:email auth google start <account>` prints a Google installed-app browser authorization URL for Gmail OAuth.
- `:email auth google finish <account> <copied-url>` completes OAuth from the full pasted failed loopback redirect URL and stores the refresh token privately.
- Omit the account to list eligible enabled, state-owned Google accounts.
  Completion requires IDs of at most 128 bytes of printable, non-whitespace ASCII.
- These actions are separate from `policy.allow_state_policy_extensions`; that policy controls whitelist actions only. `:email auth google` is refused for accounts that set `auth.refresh_token_secret`, because those accounts use a manually supplied token secret.


## Troubleshooting

If unavailable, check `extensions.std-pim.enable`,
`extensions.std-pim.config.email.enable`, account enable flags, and role tool grants.

If startup reports a missing secret, confirm the secret is declared under `extensions.std-pim.secrets` and that the secret file or `TAU_SECRET_*` variable uses the same normalized name.

Gmail IMAP/SMTP requires `https://mail.google.com/`, which Google's device flow
rejects. Use a `Desktop app` client, not Calendar's TVs/Limited Input client.
Start prints an installed-app PKCE authorization URL; after the browser fails to connect
to `http://127.0.0.1:<port>/`, paste the full address-bar URL into finish.
Testing-mode refresh tokens may expire after roughly seven days; use an
Internal/trusted Workspace or published/verified app. Workspace policy may still
block it. Remove `refresh_token_secret` before switching to state-owned OAuth.
The extension caches access tokens until near expiry and retries once after
IMAP/SMTP authentication failure. Action output excludes codes, PKCE verifiers,
and tokens.

If all reads need approval, check `trusted_authserv_ids` against raw headers.
For `dkim missing` or `auth unaligned`, check provider/list rewriting and allowlist
scope; use manual approval rather than weakening global authentication.

For tracing logs, use:

```sh
TAU_LOG='pim=debug,warn' tau
```
