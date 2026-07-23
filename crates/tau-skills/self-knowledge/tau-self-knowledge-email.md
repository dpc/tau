---
name: tau-self-knowledge-email
description: Use this skill when the user asks how to configure Tau's standard PIM/email extension, std-pim/std-email/tau-ext-pim, mail accounts, IMAP/SMTP, email approvals, incoming authentication, DKIM, Authentication-Results, or email security policy.
advertise: false
---

# Tau std-pim email configuration

Tau's preferred built-in PIM extension is named `std-pim`. It runs `tau component ext-pim`, registers split model-visible email tools such as `email_list_folders`, `email_read`, and `email_send`, and publishes `:email` approval/denial actions. The legacy `std-email` built-in alias remains for old email-only configs; do not enable both names together.

Use this skill when helping a user configure email. Do not include personal addresses, server names, passwords, authserv-ids, or message contents unless the user explicitly provided them for that answer.


## Secure baseline

Start from fail-closed settings:

- Keep `extensions.std-pim.enable: true` only when the user really wants PIM access. Legacy `extensions.std-email.enable: true` still works for old email-only configs.
- Set the email module's `config.email.enable: true`; it is false by default.
- Set each account's `enable: true`; accounts are disabled by default.
- Keep `policy.incoming_auth.require: true`; this is the default and should normally stay true.
- Keep `policy.incoming_auth.allow_dmarc_only: false`; the default requires aligned DKIM.
- Configure exact `policy.incoming_auth.trusted_authserv_ids` for the user's trusted mailbox provider or final MTA.
- Keep `incoming_allow`, `outgoing_allow`, and `folders.allow` narrow.
- Set `policy.allow_state_policy_extensions: false` if the user wants config-only allowlists and no persistent whitelist changes from `:email ... whitelist` actions.

Never recommend disabling incoming auth just to make a message pass. Prefer manual approval for edge cases.


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

Important fields:

- Preferred built-in extension name: `std-pim`; legacy alias: `std-email`.
- Model-visible email tools are split by command, for example `email_list_folders`, `email_list_recent`, `email_read`, `email_request_access`, and `email_send`.
- IMAP default: port 993 with `tls: required`.
- SMTP default: port 587 with `tls: start_tls`.
- Password auth requires `auth.password_secret` and a matching declaration under `extensions.std-pim.secrets`.
- `auth.method: none` is only for SMTP-only or relay-style setups; IMAP requires password auth or Gmail OAuth.
- Gmail OAuth uses `auth.method: oauth2`, `auth.provider: google`, `auth.client_id_secret`, optional `auth.client_secret_secret`, and either `:email auth google start <account>` plus `:email auth google finish <account> <copied-url>` for private state-owned refresh tokens or `auth.refresh_token_secret` for a manual refresh token. Other generic OAuth providers are not implied. Deprecated command-based password/token sources are rejected.
- List-style email outputs use Tau's header-then-payload tool-output shape: headers such as `format: ...` first, one empty line, then plain unindented rows. Message rows start with UID. Pass that row UID as `email_id` to message-targeting split tools. Folder ids are opaque tokens returned by `email_list_folders`; pass them back exactly as returned, without decoding or rewriting them. Attachment metadata inside `email_read` is structured detail metadata, not a top-level list response.


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

The visible `From` header is spoofable. The extension only treats an incoming allowlist match as valid when the message also has trusted authentication evidence.

By default, auto-read requires:

1. `From` matches `policy.incoming_allow` or a persisted incoming whitelist pattern.
2. The newest parsed `Authentication-Results` header has an authserv-id listed in `policy.incoming_auth.trusted_authserv_ids`.
3. That trusted header reports `dkim=pass` with `header.d` aligned to the visible `From` domain.

The extension does not cryptographically verify DKIM itself. It trusts `Authentication-Results` added by the user's mailbox provider or final trusted MTA. Tell users to inspect raw headers and copy the exact authserv-id token from a header their provider adds, for example the `mx.example.com` part of:

```text
Authentication-Results: mx.example.com; dkim=pass header.d=trusted.example; dmarc=pass header.from=trusted.example
```

Do not configure the sender domain as `trusted_authserv_ids` unless that is actually the authserv-id emitted by the trusted server.

If authentication fails, the extension returns approval reasons such as `auth missing`, `untrusted auth server`, `auth failed`, `auth unaligned`, `dkim missing`, or `auth truncated`. Those are expected fail-closed outcomes; approve individual messages manually instead of weakening global policy.


## Allowlists and patterns

Incoming and outgoing allowlists accept:

- exact addresses: `alice@example.com`
- globs: `*@example.com`
- regexes with a `re:` prefix, matched against the whole normalized address: `re:.*@trusted\.example`

Advice:

- Prefer exact addresses or narrow domains.
- Avoid broad patterns for domains where many people can send mail.
- Remember incoming allow only controls read exposure; outgoing allow controls delivery without approval.
- `reply_to` is also checked for outgoing sends.
- Bcc is checked for policy and visible to the user while a draft is pending via `:email out open <id>`, but hidden from model-facing send status and denied status output.


## Approval workflow

Incoming reads:

- `email_list_folders` returns a `format` header plus one line per visible folder. Folder ids are opaque values to pass back exactly as returned to other email tools.
- `email_list_recent` shows recent messages from IMAP internal-date search; omit `folder` to use the default folder. It returns a `format` header plus one line per message, redacts untrusted message details, and includes `access=full|preview|none`.
- `email_read` returns full body content only when access is `full`, meaning policy passes or an exact incoming approval exists. Agent-visible read bodies are simplified and wrapped in `<external_unstrusted_message>...</external_unstrusted_message>`.
- For `preview` access, `email_read` returns only a heavily stripped `body_preview`: HTML removed, links replaced with `LINK`, and only ASCII letters/digits, spaces, commas, and periods inside the wrapper. It does not ask the user for approval.
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

- Agent `email_list_recent`, `email_read`, `email_request_access`, `email_send`, `email_mark_read`, `email_mark_unread`, `email_star`, `email_unstar`, and `email_trash` activity is appended as sanitized JSONL under the email state directory.
- Use `:email log last [number]` to review recent activity; the number defaults to 20.
- The pretty log is intentionally minimal and does not include message bodies.

Whitelist actions:

- `:email in whitelist <pattern>` persists an incoming pattern.
- `:email out whitelist <pattern>` persists an outgoing pattern.
- These actions only affect policy when `policy.allow_state_policy_extensions` is true.

Google OAuth actions:

- `:email auth google start <account>` prints a Google installed-app browser authorization URL for Gmail OAuth.
- `:email auth google finish <account> <copied-url>` completes OAuth from the full pasted failed loopback redirect URL and stores the refresh token privately.
- Google auth account arguments complete from enabled state-owned Google email accounts whose ids are at most 128 bytes of printable, non-whitespace ASCII. Omitting the account lists the bounded sorted eligible inventory, or states explicitly that no eligible accounts are available.
- These actions are separate from `policy.allow_state_policy_extensions`; that policy controls whitelist actions only. `:email auth google` is refused for accounts that set `auth.refresh_token_secret`, because those accounts use a manually supplied token secret.


## Troubleshooting

If the extension is unavailable, check both enable flags: `extensions.std-pim.enable` and `extensions.std-pim.config.email.enable`.

If startup reports a missing secret, confirm the secret is declared under `extensions.std-pim.secrets` and that the secret file or `TAU_SECRET_*` variable uses the same normalized name.

For Gmail OAuth, Gmail IMAP/SMTP requires the broad `https://mail.google.com/` scope, and Google's device flow rejects that scope. Use a Google OAuth client of type `Desktop app`; do not reuse the Calendar TVs/Limited Input device-flow client for Gmail. Start prints a URL, the browser eventually fails to connect to `http://127.0.0.1:<port>/`, and finish expects the full copied address-bar URL. Google OAuth apps left in Testing mode may issue refresh tokens that expire after roughly 7 days for sensitive/restricted scopes; real use should use an Internal/trusted Workspace app or a properly published/verified app. Workspace administrators may still block untrusted OAuth apps. If `refresh_token_secret` is configured, `:email auth google` refuses to overwrite it; remove that field first to use state-owned OAuth.

If all incoming reads require approval, inspect raw message headers and configure `trusted_authserv_ids` for the trusted provider's authserv-id. With `incoming_auth.require: true` and no trusted authserv-id, fail-closed approval is expected.

If a legitimate sender fails with `dkim missing` or `auth unaligned`, do not immediately disable auth. First check whether the provider rewrites mail, a list forwarder changed authentication alignment, or the allowlist is too broad. Manual approval is safer than global weakening.

For tracing logs, use:

```sh
TAU_EXT_LOG=pim=debug tau
```
