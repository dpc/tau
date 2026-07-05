# tau-ext-telegram security notes

- The built-in extension is disabled by default and requires an explicit bot
  token secret and non-empty `allowed_user_ids` allowlist.
- Telegram bot tokens are secrets. Do not derive `Debug` for structs containing
  token text and do not include Bot API URLs in error strings.
- The model cannot choose arbitrary chat ids. The send tool (`telegram_send` for
  the legacy `std-telegram` instance, or the instance's namespaced send tool)
  uses only the configured `chat_id` or the single allowlisted private chat that
  linked itself with `/start`.
- Messages from users outside `allowed_user_ids` are ignored before any routing
  or Telegram reply side effects.
- Only one Telegram chat is active at a time. With a configured `chat_id`,
  messages from other chats are refused; without one, prompt-routing text and
  commands are refused until one allowlisted private chat links with `/start`,
  and other chats cannot replace that link.
- Unconfigured group/supergroup chats are refused. Groups are accepted only when
  their `chat_id` is explicitly configured by the user.
- Text is treated as untrusted user input and is prefixed with Telegram source
  context before being submitted to Tau.
- Reconfiguration fails closed. If a new config cannot be parsed or validated,
  the active runtime config, registrations, selected agents, learned chat, and
  update offset state are cleared until a valid config is applied and agents
  explicitly register again.
- `getUpdates` polling is protected by an advisory OS lock keyed by Bot API base
  plus bot token for Tau processes sharing the same Tau state root. Lock
  sidecars and contention diagnostics include only the API base, owner metadata,
  and a non-secret stream hash, never the raw bot token.
- The idle-to-active polling transition checks `getWebhookInfo` after taking the
  local lock. Active webhooks fail registration visibly because Telegram will
  not serve `getUpdates`; Tau must not automatically delete the webhook or
  request dropping pending updates.
- Telegram HTTP 409 `getUpdates` conflicts indicate webhook mode changes or an
  out-of-band long-poll consumer. They are surfaced as user-visible notices and
  clear active registrations instead of silently leaving the bridge apparently
  connected.
- In-flight poll responses captured under an older configuration are discarded
  so old Telegram streams cannot advance offsets, mark backlog draining complete,
  send replies, or submit prompts after reconfiguration.
- The standalone `tau-telegram-gateway` MVP is a stream owner and status daemon
  only. It reads the bot token from an environment variable, shares the same
  stream lock and webhook/409 diagnostics, stores durable per-stream offset and
  duplicate-suppression state under a private state directory, and exposes only
  private same-UID local IPC. The socket supports one-shot `status` and
  persistent sidecar `hello`/`heartbeat`/`register_agent`/`unregister_agent`/
  `goodbye`; registered routes are live leases pruned on unregister, goodbye,
  disconnect, protocol-error disconnect, heartbeat expiry, or gateway restart
  reannouncement. The socket bounds request size but is same-UID local IPC, not a
  sandbox or DoS boundary against the user's own processes. It handles `/start`,
  `/help`, and `/status`; it does not submit Telegram text to Tau sessions or
  send outbound `telegram_send` messages through the gateway yet.
- Gateway mode must continue to reject non-allowlisted Telegram users before any
  side effects. Without an explicit configured `chat_id`, group/supergroup chats
  must not be linked or replied to; only an allowlisted private chat may link
  with `/start`.
- If `api_base` is overridden for tests, production endpoints must use HTTPS.
  Plaintext HTTP is accepted only for loopback hosts, and userinfo, query, and
  fragment components are rejected because the bot token is embedded in request
  paths.
