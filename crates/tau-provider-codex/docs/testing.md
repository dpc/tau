# Testing tau-provider-codex

Parser and streaming changes use focused event, delta, snapshot, and golden-request
tests. WebSocket changes cover pool identity, reservation and release, reconnect,
transport-only `websocket_control_ping` control frames, idle timeout, typed
cancellation, and abort wakeups without short polling. Resource-policy
regressions drive production localhost sockets through exact and first-excess
1 MiB frame and fragmented complete-message boundaries, plus the exact and
first-excess 64 MiB cumulative-attempt boundary. They assert task retirement,
pre-parse fixed terminal classification, and exact retained-state accounting
for slots, assistant/reasoning/tool text, terminal data, and opaque raw replay.
The bounded-lane tests fill the sole provider-event slot, prove saturation
backpressures rather than drops, then drain events in wire order.
Control-priority tests queue provider data and prove coalesced cancellation and
writer-failure wakes preempt it.

Local peers join the production upgrade, request lowering, background tasks, frame
parsing, and typed error mapping. They bind only loopback, use synthetic
credentials, bound connections and frames, synchronize explicitly rather than with
sleeps, and join workers at teardown. Prewarm coverage includes silent peers,
duplicate admission, cancellation cleanup, invalidation races, and socket reuse.
It also proves exact first-send dispatch, cumulative bytes across the sole
pre-semantic repair, canonical stale/connection-limit precedence, no replay after
semantic progress, strict same-socket prefix/fingerprint chain eligibility, and
stale-generation publication rejection.

Ordinary response-chain coverage is separate from prewarm. A gated same-socket
turn proves that input committed while a response is in flight invalidates that
response's causal-prefix proof, forces an ordered full replay, and publishes a
new anchor only after success; companion cases retain compatible incremental
reuse and tool-result continuation. VCR has no live socket proof and reconstructs
request shape only: replay-only tests require exact matching against both the
recordable full-replay shape and the compatible chained-delta shape.

Changes to a default route or protocol surface maintain a capability matrix for
direct function and custom calls, parallel generation, programmatic/code mode,
hosted tools, images and detail, reasoning continuity, compaction, chaining and
replay, WebSocket inference, unary HTTPS, quota/retry behavior, and profile/auth scope. Golden
requests cover every supported mode. Prompts must not advertise unsupported
capabilities, and the default retains an end-to-end multi-tool lifecycle test.
Reference-client metadata is evidence rather than a Tau requirement; compatibility
modes do not become retry fallbacks without a separately approved decision.
The reviewed [`compat` request fixtures](../fixtures/compat/README.md) freeze full
Standard and Lite lowering from production model configuration.

Curated provider evidence follows
[`SPEC-tau-provider-codex-curated-vcr`](../specs/SPEC-tau-provider-codex-curated-vcr.md).
Workspace-wide response-streaming guidance remains in
[`docs/testing.md`](../../../docs/testing.md).
