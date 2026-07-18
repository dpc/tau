# Testing tau-provider-chatgpt

Parser and streaming changes use focused event, delta, snapshot, and golden-request
tests. WebSocket changes cover pool identity, reservation and release, reconnect,
idle timeout, typed cancellation, and abort wakeups without short polling.

Local peers join the production upgrade, request lowering, background tasks, frame
parsing, and typed error mapping. They bind only loopback, use synthetic
credentials, bound connections and frames, synchronize explicitly rather than with
sleeps, and join workers at teardown. Prewarm coverage includes silent peers,
duplicate admission, cancellation cleanup, invalidation races, and socket reuse.

Changes to a default route or protocol surface maintain a capability matrix for
direct function and custom calls, parallel generation, programmatic/code mode,
hosted tools, images and detail, reasoning continuity, compaction, chaining and
replay, HTTP and WebSocket, quota/retry/fallback, and profile/auth scope. Golden
requests cover every supported mode. Prompts must not advertise unsupported
capabilities, and the default retains an end-to-end multi-tool lifecycle test.
Reference-client metadata is evidence rather than a Tau requirement; compatibility
modes do not become retry fallbacks without a separately approved decision.
The reviewed [`compat` request fixtures](../fixtures/compat/README.md) freeze full
Standard and Lite lowering from production model configuration.

Curated provider evidence follows
[`SPEC-tau-provider-chatgpt-curated-vcr`](../specs/SPEC-tau-provider-chatgpt-curated-vcr.md).
Workspace-wide response-streaming guidance remains in
[`docs/testing.md`](../../../docs/testing.md).
