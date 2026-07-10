# tau-ext-slack security notes

- The built-in extension is disabled by default and requires explicit app-token
  and bot-token secrets plus a non-empty `allowed_user_ids` allowlist.
- Slack app tokens (`xapp-...`), bot tokens (`xoxb-...`), and Socket Mode
  websocket URLs are secrets. Do not derive `Debug` for structs containing token
  text and never log websocket URLs.
- The model cannot choose arbitrary Slack destinations. `slack_send` has no
  destination argument and uses only the configured channel or single
  allowlisted DM established by the accepted/started lifecycle for the calling
  registered agent's active prompt, plus its validated originating thread root
  when present. It fails
  before such an authorized origin exists and never accepts model-supplied
  `thread_ts`.
- Authorization activates only after matching live submitted/started lifecycle
  facts. An unrelated submitted prompt or any steered prompt revokes it; a
  context-less tool-result follow-up preserves it only when no user prompt
  intervened. Queued prompts folded as steers never authorize a destination.
- Only `app_mention` events from ids explicitly listed in `channel_ids` route
  channel prompts. Unconfigured channels and DMs are ignored without reply side
  effects. With an empty list, one allowlisted DM can link with `start` and route
  direct `message` events.
- Messages from users outside `allowed_user_ids` are ignored before any routing
  or Slack reply side effects.
- Reactions route only for allowlisted human users, authorized conversations,
  and bounded in-memory `(channel, message timestamp)` identities returned by
  Slack for successful `slack_send` posts. Reactions to arbitrary posts,
  stale/unregistered owners, bot-self reactions, and retries are ignored.
  Human-account status is checked fail-closed with `users.info`; reaction
  support therefore requires the `users:read` bot scope.
- Slack text is untrusted prompt input and can contain prompt injection. Tau
  prefixes it with compact Slack source context before submitting it as a normal
  prompt request.
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
