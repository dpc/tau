# DESIGN-tau-harness-manual-compaction: Model-callable manual compaction

Status: unconfirmed

`compact` and `agent_compact` are independent disabled-by-default internal
tools. Prompt-snapshot presence is the capability: `compact` targets only its
owner, and `agent_compact` targets any other loaded same-session agent without
an ancestry test. The harness records a bounded request id, caller, target,
prompt, tool call, model, and accepted head before returning its background
placeholder. Replay either waits for a complete self tool round, starts the
transaction once, or reconstructs its missing terminal background completion.

Testing ownership is split by boundary: `tau-harness-tools` owns schemas,
strict parsers, and independent groups; `tau-core` owns request/start/failure
validation plus durable and memory-only replay; `tau-harness` owns prompt
snapshot authority, loaded-target and state matrices, complete sibling-round
ordering, arbitration, provider terminals, cancellation, crash repair,
exactly-once background completion, watcher sanitization, and continuation
checkpoints.
