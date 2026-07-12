# DESIGN-tau-harness-watch-endpoint-retirement: Agent unload retires watch endpoint state

Status: unconfirmed

A committed unload of either endpoint retires every incoming and outgoing
watch relation, subscription identity, current provider snapshot, and
per-subscription delivery state. The local removal fallback is idempotent with
the committed reaction. Later fanout requires both endpoints to remain live,
surviving watchers receive an authoritative replacement snapshot, and loading
the same agent again requires a fresh subscription. Harness durable-log tests
cover both endpoint directions, replacement topology, absence of post-unload
retry/recovery/terminal fanout, and fresh state after same-session reload.
