# tau-ext-slack security notes

Slack `listening_scope` defaults to `mentions_only`; `all_messages` expands only trigger scope in authorized conversations. Verified-human, strict/lax sender policy, bot/self denial, untrusted content, and source-bound reply authorization remain unchanged. Duplicate `message` and `app_mention` delivery of one `(channel, ts)` shares durable dedup identity.

- The built-in extension is disabled by default and requires explicit app-token
  and bot-token secrets plus a non-empty `allowed_user_ids` allowlist.
- Slack app tokens (`xapp-...`), bot tokens (`xoxb-...`), and Socket Mode
  websocket URLs are secrets. Do not derive `Debug` for structs containing token
  text and never log websocket URLs.
- The model cannot choose arbitrary Slack destinations. `slack_send` requires
  an opaque canonical `reply_to` selector and accepts no channel, user, or
  thread argument. Both extension and harness revalidate the live
  connection/session/agent/tool and exact source-bound actor/conversation/thread.
- Authorization activates only after the harness reports durable ingress
  commit. Retry returns the same id; replay cannot activate routes or wake an
  agent. Unregister, unload, extension/harness disconnect, shutdown, and
  reconfiguration revoke runtime-only routes. Socket Mode websocket reconnects
  preserve routes in the same authenticated extension/session. Reply routes and
  pending completions are each capped at 1024 entries.
  Session changes clear routes and use a new correlation id to renew the
  source-bound capability; late results from earlier sessions are ignored.
- In default `mentions_only`, configured channels route `app_mention`; in
  `all_messages` they also route ordinary `message` events. Unconfigured channels
  and DMs are ignored without reply side effects. With an empty list, one
  allowlisted DM can link with `start` and route direct `message` events in either
  scope.
- `security_mode` defaults to `strict`, where users outside `allowed_user_ids`
  are ignored. `lax` admits verified non-bot humans only in an already configured
  channel or linked DM, including their edits and owned-post reactions. They
  cannot link, select agents, or run bridge commands. Lax expands prompt-injection
  exposure; it grants no bridge-control or destination-selection authority; accepted ingress activates only its authenticated source-bound reply route.
- Reactions route only for policy-permitted verified human users, authorized conversations,
  and bounded in-memory `(channel, message timestamp)` identities returned by
  Slack for successful `slack_send` posts. Reactions to arbitrary posts,
  stale/unregistered owners, bot-self reactions, and retries are ignored.
  Human-account status is checked fail-closed with `users.info` for creates,
  edits, and reactions; all ingress therefore requires the `users:read` bot
  scope.
- `message_changed` routes only when its original incoming create committed and
  remains in the bounded native-identity cache. Channel, thread, original
  sender/editor, message timestamp, and revision metadata must agree exactly;
  unknown or conflicting edits fail closed and never become new creates.
- Slack text is untrusted external content and can contain prompt injection. It
  stays an unprefixed payload in a harness-stamped typed envelope; allowlist and
  verified-account metadata do not make its content trusted.
  Typed identity assurance, allowlist/policy status, and content trust remain
  separate; escaped provider lowering prevents payload lookalike tags from
  forging any of them.
- Slack workspace admins, Slack itself, channel members, and Slack Connect
  participants with access to a channel may be able to read messages. This MVP
  does not provide end-to-end encryption.
- Reconfiguration fails closed. Before worker startup, parse/validation errors
  are reported as `ConfigError` and clear inactive config, registrations,
  selected agents, and learned DM state. After worker startup, config changes are
  rejected with `ConfigError`; restart Tau to apply them.
- If `api_base` is overridden for tests, production endpoints must use HTTPS.
  Plaintext HTTP is accepted only for loopback hosts, and userinfo, query, and
  fragment components are rejected.
- Returned Socket Mode websocket URLs are validated and never logged. Production
  websocket URLs must use WSS; plaintext WS is accepted only for loopback tests.
- All model-visible and log-visible diagnostics are bounded and token-redacted.
