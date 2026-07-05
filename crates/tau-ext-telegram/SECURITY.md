# tau-ext-telegram security notes

- The built-in extension is disabled by default and requires an explicit bot
  token secret and non-empty `allowed_user_ids` allowlist.
- Telegram bot tokens are secrets. Do not derive `Debug` for structs containing
  token text and do not include Bot API URLs in error strings.
- The model cannot choose arbitrary chat ids. `telegram_send` uses only the
  configured `chat_id` or the single allowlisted private chat that linked itself
  with `/start`.
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
- In-flight poll responses captured under an older configuration are discarded
  so old Telegram streams cannot advance offsets, mark backlog draining complete,
  send replies, or submit prompts after reconfiguration.
- If `api_base` is overridden for tests, production endpoints must use HTTPS.
  Plaintext HTTP is accepted only for loopback hosts, and userinfo, query, and
  fragment components are rejected because the bot token is embedded in request
  paths.
