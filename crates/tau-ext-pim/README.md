# tau-ext-pim

`tau-ext-pim` is Tau's standard personal information management extension. It exposes split model-visible email tools such as `email_list_folders`, `email_read`, and `email_send`, plus split calendar tools such as `calendar_search` and `calendar_create`. `/email` and `/calendar` slash actions handle user review and approvals.

The preferred built-in extension name is `std-pim`. The legacy `std-email` alias remains for existing email-only configs. Both are disabled by default and must be explicitly enabled in `harness.yaml`; enable only one of them.


## Security model and hardening

Email is hostile input. Message bodies, subjects, display names, addresses, MIME headers, attachment names, folder names, backend errors, and provider-added metadata can contain prompt injection, terminal control bytes, misleading Unicode, huge payloads, or spoofed identity data. The extension is built to expose as little as possible to the model by default and to make unsafe cases require explicit user action.

### Default stance

- The built-in `std-pim` extension is disabled by default.
- Each module's own `enable` flag is also false by default.
- Accounts are disabled unless `account.enable: true` is set.
- Folder visibility is deny-by-default: if `folders.allow` is empty, no folders are visible or selectable.
- Incoming sender allow policy is empty by default.
- Outgoing recipient allow policy is empty by default.
- Incoming authentication is required by default with `policy.incoming_auth.require: true`.
- Aligned DKIM is required by default. `policy.incoming_auth.allow_dmarc_only` defaults to false.
- With no trusted `Authentication-Results` authserv-id configured, incoming messages fail closed and require approval even if the sender address matches `incoming_allow`.

### Incoming email gating

`email_list_recent` returns bounded line-oriented metadata with a `format` header and one line per message. Each line includes an `access` field: `full`, `preview`, or `none`. `full` means `email_read` can return simplified full content. `preview` means `email_read` returns only a stripped `body_preview`. `none` means `email_read` returns an `approval_required` error and no body preview.

The list format is `uid date from flags access attachments subject...`. For messages that do not have `full` access, `flags` includes `redacted`, attachment metadata is `?`, and `subject...` is only a short lossy preview containing ASCII letters/digits, commas, semicolons, periods, spaces, and dashes.

`email_read` first fetches bounded headers and makes a policy decision before exposing body-like text to the model. In `preview` mode, it returns a heavily stripped `body_preview` without creating a user approval request. The preview has HTML removed, links replaced with `LINK`, and only ASCII letters/digits, spaces, commas, and periods inside the wrapper. If the preview justifies full access, the agent can call `email_request_access` for the same message to create an incoming approval. The user can inspect the message with `/email in open <id>`, approve it with `/email in approve <id>`, or deny the exact request with `/email in deny <id>`. After approval, the model must repeat the matching `email_read` call to fetch the content. After denial, matching reads report `access=none`; an explicit `email_request_access` can still ask the user again.

Incoming approval records are bound to account, folder, UID, UIDVALIDITY when available, normalized sender, date, and message-id. Approval is not just a free-floating id that can be reused for a different message.

### From spoofing and Authentication-Results

The visible `From` address is not trustworthy by itself. The default policy requires two things before a whitelisted sender can be auto-read:

1. The normalized `From` address must match `policy.incoming_allow` or a persisted incoming whitelist pattern.
2. The newest parsed `Authentication-Results` header must come from a configured trusted authserv-id and show an aligned DKIM pass for the visible `From` domain.

This extension does not cryptographically verify DKIM signatures itself. It consumes `Authentication-Results` produced by your mailbox provider or final trusted MTA. That means the trust boundary is your mail server. Only configure authserv-id values for a server that you trust to add its own authentication results and to handle attacker-supplied lower `Authentication-Results` headers safely.

The extension trusts only the topmost parsed `Authentication-Results` header. Lower headers can be forged by senders or inserted by intermediate relays, so they are ignored for auto-read decisions even if they look favorable.

Use `policy.incoming_auth.trusted_authserv_ids` for exact authserv-id values, such as the leading token in a raw header like `Authentication-Results: mx.example.com; dkim=pass ...`. Do not put sender domains there unless that is actually the authserv-id emitted by your trusted server.

Important failure reasons include:

- `untrusted` — sender did not match incoming allow policy.
- `auth missing` — no usable `Authentication-Results` evidence was found.
- `untrusted auth server` — the newest auth header came from an authserv-id not in `trusted_authserv_ids`.
- `auth failed` — trusted evidence was present but did not pass.
- `auth unaligned` — authentication passed for some other domain.
- `dkim missing` — DMARC passed, but aligned DKIM did not pass and DMARC-only mode is disabled.
- `auth truncated` — the bounded metadata fetch was truncated, so authentication evidence may be incomplete.

### Authentication policy choices

Keep these defaults unless you have a clear reason:

```yaml
policy:
  incoming_auth:
    require: true
    trusted_authserv_ids:
      - mx.example.com
    allow_dmarc_only: false
```

Setting `require: false` means an incoming sender allowlist match can expose message bodies without DKIM or trusted `Authentication-Results`. That is unsafe for most users.

Setting `allow_dmarc_only: true` allows aligned DMARC pass without aligned DKIM. This can be useful for some forwarding or provider setups, but it is weaker than the default. Prefer approving those messages manually instead of weakening policy globally.

### Prompt injection remains possible

A message that passes policy is authenticated as coming from an allowed sender; it is not safe. The body can still instruct the agent to ignore rules, reveal secrets, send mail, run tools, or manipulate the user. Treat email content as user-supplied data, not as system instructions. Model-visible read bodies and unapproved previews are simplified and wrapped in `<external_unstrusted_message>...</external_unstrusted_message>` to mark the hostile boundary; the wrapper is not a safety guarantee.

The extension reduces accidental exposure, but it cannot make email content semantically safe. Users should review surprising content and keep allowlists narrow.

### Display and output hardening

The extension sanitizes model-facing and action-list text derived from email. Control characters, escape bytes, bidirectional formatting controls, newlines, and very long display fields are escaped or capped before display. Approved read bodies are simplified by stripping HTML, script/style/head/svg blocks, links, obvious quoted replies, and common signatures/disclaimers. Unapproved body previews are stricter: they remove HTML, replace links with `LINK`, cap length, and allow only ASCII letters/digits, spaces, commas, and periods inside the wrapper. Unapproved subject previews are short, ASCII-only, and limited to letters/digits plus `,`, `;`, `.`, space, and `-`. This is important because approval lists and status messages may be rendered in a terminal.

Model-visible incoming `From` values are normalized to the address instead of trusting arbitrary display names. Raw authentication headers are not exposed to the model. Backend errors are capped before being returned.

### Bounded IMAP access

Metadata and body fetches are bounded. Header fetches use a fixed byte window, body reads have a byte and line cap, and outputs mark truncation. `email_list_recent` uses IMAP `UID SEARCH SINCE` against server internal dates, pages matching UIDs, and sorts the fetched page by internal date descending. `email_id` and folder arguments are validated before use, and returned UIDs are checked against the requested message.

If authentication headers are truncated during the metadata fetch, the extension denies auto-read with `auth truncated` instead of guessing.

### Outgoing email safety

`email_send` sends immediately only when every recipient is allowed by outgoing policy. Recipients in `to`, `cc`, `bcc`, and `reply_to` are checked. If any recipient is untrusted, the whole draft is queued for approval; the extension never does a partial send to just the allowed recipients.

Outgoing `from` cannot be spoofed. It must match the configured account identity. Unsafe or oversized recipients, subjects, bodies, and threading headers are rejected instead of being silently truncated.

Queued outgoing approvals persist the full draft for user review. Bcc recipients are hidden from model-facing status output, but visible to the user in `/email out open <id>` before approval. Approved drafts enter a `sending` state and are revalidated against the current account and policy before SMTP delivery to reduce duplicate sends and stale approval abuse.

### Approval state and allowlists

Approval files are validated on load and written atomically without overwriting existing records on id collision. Incoming and outgoing approval ids should still be treated as sensitive user-interface tokens: do not ask the model to invent or reuse them.

Agent email access and mutation commands append sanitized JSONL entries to `logs/email.jsonl` under the extension state directory. Use `/email log last [number]` to review recent `email_list_recent`, `email_read`, `email_request_access`, `email_send`, `email_mark_read`, `email_mark_unread`, `email_star`, `email_unstar`, and `email_trash` activity without exposing message bodies.

The `/email in whitelist <pattern>` and `/email out whitelist <pattern>` actions persist additional allowlist patterns when `policy.allow_state_policy_extensions` is true. This is convenient, but it means UI actions can extend policy outside the static config file. Set it to false if you want config-only policy:

```yaml
policy:
  allow_state_policy_extensions: false
```

### Secrets and credentials

Passwords are delivered through Tau extension secrets. Declare each secret under the enabled extension's `secrets`, then reference it with `auth.password_secret` in the account. Secrets are sent only to the enabled extension instance during configuration and are never returned by the tool.

Deprecated password sources such as `auth.password_env`, `auth.command`, `auth.password_command`, and OAuth command placeholders are rejected. This avoids leaking credentials through child-process arguments, inherited environments, logs, or model-visible config.

Use TLS defaults unless you are connecting to a trusted local relay:

- IMAP defaults to implicit TLS on port 993 with `tls: required`.
- SMTP defaults to STARTTLS on port 587 with `tls: start_tls`.
- `tls: none` should only be used for local test servers or a trusted local relay.

### Folder scope

Expose only the folders the agent actually needs. A narrow allowlist such as `INBOX` is safer than a broad `*`. Folder names from config and tool arguments are validated and unsafe folder values are rejected.


## Configuration

Put configuration in `~/.config/tau/harness.yaml` or a drop-in under `~/.config/tau/harness.d/`. Existing `std-email` configs with the old top-level email shape are still accepted as a compatibility path, but do not enable `std-pim` and `std-email` together.

```yaml
extensions:
  std-pim:
    enable: true
    secrets:
      mail_password: {}
      personal_calendar_ics_url: {}
      google_calendar_client_id: {}
      google_calendar_client_secret:
        optional: true
      google_calendar_refresh_token: {}
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
            folders:
              allow:
                - INBOX
                - Archive/*
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
      calendar:
        enable: true
        accounts:
          - id: personal-calendar
            enable: true
            display_name: Personal calendar
            backend:
              type: ics_feed
              url_secret: personal_calendar_ics_url
              # Non-loopback http:// feeds require allow_plain_http: true.
              # Prefer https:// or webcal:// for private feeds.
            calendars:
              default: main
              allow:
                - main
        policy:
          read:
            private_events: busy_only
            descriptions: approved_only
          write:
            require_approval: true
            max_attendees: 50
```

The `calendar.accounts[*].backend.type: google` backend uses the native Google Calendar API for reads and user-approved writes. Configure a Google OAuth client id secret, optionally a client secret, and either omit `refresh_token_secret` to authorize interactively with `/calendar auth google start <account>` followed by `/calendar auth google finish <account>`, or provide a refresh-token secret manually:

```yaml
- id: google-calendar
  enable: true
  display_name: Google Calendar
  backend:
    type: google
    client_id_secret: google_calendar_client_id
    client_secret_secret: google_calendar_client_secret # optional for installed clients
    # Omit refresh_token_secret and run /calendar auth google start google-calendar
    # then /calendar auth google finish google-calendar.
    # refresh_token_secret: google_calendar_refresh_token
  calendars:
    default: primary
    allow:
      - primary
```

For Google accounts, `calendars.allow` entries are exact Google calendar IDs; use `primary` for Google's primary-calendar alias. Display summaries are not access-control identifiers. `/calendar auth google start <account>` prints a Google verification URL and user code; after approving in the browser, run `/calendar auth google finish <account>` to store the refresh token in the extension's private state directory. The device flow requests the full Google Calendar scope because Google's device endpoint rejects some narrower Calendar scopes; manual refresh tokens must include equivalent access. The action events are transient and never include the refresh token. Manual `refresh_token_secret` config remains available for power users. The backend caches short-lived Google access tokens in memory until near expiry.

Calendar tool reads and write requests append sanitized audit entries to `logs/calendar.jsonl` under the extension state directory. Review them with `/calendar log last [number]`. Entries include command, status, account, calendar, event id, time bounds, and result counts; they do not persist event titles or descriptions. Pending calendar mutations are stored separately for `/calendar change` review.

Create the secret value as raw UTF-8 text. Despite the `.yaml` suffix, the secret file is read as trimmed text, not as a structured YAML document.

```sh
mkdir -p ~/.local/state/tau/secrets
printf '%s\n' 'app-password-or-token' > ~/.local/state/tau/secrets/mail_password.yaml
printf '%s\n' 'https://example.com/private-calendar.ics' > ~/.local/state/tau/secrets/personal_calendar_ics_url.yaml
printf '%s\n' 'google-oauth-client-id' > ~/.local/state/tau/secrets/google_calendar_client_id.yaml
printf '%s\n' 'google-oauth-client-secret' > ~/.local/state/tau/secrets/google_calendar_client_secret.yaml
printf '%s\n' 'google-oauth-refresh-token' > ~/.local/state/tau/secrets/google_calendar_refresh_token.yaml
chmod 600 ~/.local/state/tau/secrets/*.yaml
```

For one-shot startup, an environment variable also works. The suffix is normalized to the secret name.

```sh
TAU_SECRET_MAIL_PASSWORD='app-password-or-token' tau
```


## Address and folder patterns

Incoming and outgoing allowlists accept:

- exact addresses: `alice@example.com`
- glob patterns: `*@example.com`
- regular expressions with a `re:` prefix, matched against the whole normalized address: `re:.*@trusted\.example`

Patterns with control or unsafe formatting characters are rejected. Exact addresses are normalized before matching.

Folder allowlists are glob patterns over mailbox folder names. Empty `folders.allow` means no folders are visible.


## Tool commands

Email is exposed as split model-visible tools:

- `email_list_folders` — returns `format: folder flags` plus one line per visible folder. Folder ids are opaque single-column tokens; pass the returned token back exactly as shown.
- `email_list_recent`
- `email_read`
- `email_request_access`
- `email_mark_read`
- `email_mark_unread`
- `email_star`
- `email_unstar`
- `email_trash`
- `email_send`

`email_list_recent` accepts optional `folder`, `limit`, `cursor`, and `days`; omitted `folder` defaults to the default folder, and `days` defaults to 7. List-style commands return a `format` header and one safe line per listed item, with the follow-up key first and whitespace/control-safe fields so rows cannot forge extra columns or lines; reuse opaque id fields exactly as returned. Message-targeting tools (`email_read`, `email_request_access`, `email_mark_read`, `email_mark_unread`, `email_star`, `email_unstar`, and `email_trash`) take the same `folder` plus `email_id` target; pass the list row `uid` value as `email_id`. `email_request_access` creates or reuses a pending incoming approval so the user can decide whether the agent may read the full message. Message-management commands do not require content approval. `email_trash` moves the message to Trash.

Calendar is exposed as split model-visible tools:

- `calendar_list_calendars` — returns visible calendars as opaque ids for other calendar tools.
- `calendar_search` — lists bounded event metadata for one calendar id.
- `calendar_get` — reads one event by `event_id`; Google ETags are cached internally for safe writes.
- `calendar_free_busy` — returns busy blocks without descriptions.
- `calendar_create` — queues or applies a Google event create request.
- `calendar_update` — queues or applies a Google event patch; requires `event_id` and at least one changed field.
- `calendar_delete` — queues or applies a Google event delete; requires `event_id`.
- `calendar_respond` — queues or applies an RSVP response; requires `event_id` and `response`.

Calendar list-style results render as headers, one blank line, then plain unindented rows. Headers include `format`; `calendar_search` and `calendar_free_busy` also include `next_cursor` and `truncated`, so pass the cursor with the same calendar/range arguments to continue. If a list has no rows, the payload is `(no matches found)`.

Calendar writes target Google accounts only. The default write policy queues `/calendar change` approvals; ICS feed accounts remain read-only. `calendar_search` accepts an optional `title` substring filter. `start` and `end` accept RFC3339 date-times with offsets, local `YYYY-MM-DDTHH:MM:SS` date-times interpreted in the configured or system timezone, natural expressions like `today`, `tomorrow`, or `next week`, and `YYYY-MM-DD` all-day dates.

## User approval actions

The extension publishes `/email` actions for review:

- `/email log last [number]` — show recent agent email access and mutation log entries; defaults to 20.
- `/email in list` — list pending incoming read approvals.
- `/email in open <id>` — inspect an incoming message; may display email content to the user.
- `/email in approve <id> [id...]` — approve exact incoming reads.
- `/email in deny <id> [id...]` — deny exact incoming reads; future `email_read` calls report `access=none`, while explicit `email_request_access` calls can ask again.
- `/email in whitelist <pattern>` — persist an incoming allow pattern, if state policy extensions are enabled.
- `/email out list` — list pending outgoing drafts.
- `/email out open <id>` — inspect an outgoing draft, including Bcc.
- `/email out approve <id> [id...]` — send the approved draft(s).
- `/email out whitelist <pattern>` — persist an outgoing recipient allow pattern, if state policy extensions are enabled.

The extension also publishes `/calendar` actions:

- `/calendar auth google start <account>` — print a Google verification URL and user code for OAuth device authorization.
- `/calendar auth google finish <account>` — complete OAuth after browser approval and store the refresh token privately.
- `/calendar log last [number]` — show recent calendar access and mutation log entries; defaults to 20.
- `/calendar change list` — list pending calendar mutations.
- `/calendar change open <id>` — inspect a pending calendar mutation.
- `/calendar change approve <id> [id...]` — apply approved Google calendar mutation(s).
- `/calendar change deny <id> [id...]` — deny pending calendar mutation(s).


## Tracing

The extension uses the `pim` tracing target:

```sh
TAU_EXT_LOG=pim=debug tau
```
