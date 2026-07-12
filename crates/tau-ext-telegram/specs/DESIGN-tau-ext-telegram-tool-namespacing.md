# DESIGN-tau-ext-telegram-tool-namespacing: Telegram tool names are instance-namespaced outside `std-telegram`

Status: unconfirmed

Tau tool names are global within one harness prompt/routing surface. Multiple
Telegram extension instances with distinct bot tokens therefore must not all
publish `telegram_register` and `telegram_send`. The built-in `std-telegram`
instance keeps those historical names and group `telegram`; any other instance
derives a collision-free namespace from the configured extension instance name
by escaping underscores as `__` and hyphens as `_d`, unless
`config.tool_namespace` explicitly sets a valid ASCII tool namespace. The
resulting tools are `<namespace>_register` and `<namespace>_send`, with group
`<namespace>`.

The namespace is computed from initial configuration before `Ready` and cannot
change on runtime reconfiguration because tool declarations are startup
declarations. Per-token update-stream locking remains independent of tool
namespacing, so accidental token reuse between differently named instances still
fails closed.
