# SPEC-provider-execution-reports-and-canonical-facts: Provider execution flow

## Record justification

Provider execution spans protocol wire authority, extension activation and
interception, harness prompt/retry correlation, durable response processing, provider
implementations, and UI/replay consumers. No single crate can state the complete
commit-before-semantics contract or the terminal alternatives coherently.

Configured Provider extensions submit
`provider.prompt_submitted_reported`, `provider.response_updated_reported`,
`provider.response_finished_reported`, `provider.retry_prompt_result_reported`, and
`provider.cache_miss_diagnostic_reported`. Each reuses the corresponding provider
payload DTO. Reports use ordinary generic Emit admission, interception, commit, and
broadcast before the harness performs prompt or retry correlation.

Only the exact live configured Provider generation may author these reports. Generic
admission preserves the submitted `persist` bit. Reports default to `persist=false` and are
categorically excluded from agent, session, and restore history for either bit. The
runtime log retains their observation order, but replay never reruns report semantics.

Pre-Ready staging remains source-bound and preserves the complete encoded envelope and
global activation ordering. On release, the publication captures the stable configured
publisher, source connection, Provider kind, and logical instance before interception.
Downstream processing requires that exact generation to remain current. A report parked
across disconnect or replacement may commit but cannot mutate current state or derive a
successor. A same-name interceptor replacement reruns every correlation check; dropping
a report has no semantic effect.

## Canonical facts and correlation

No peer may author canonical `provider.prompt_submitted`,
`provider.response_updated`, `provider.response_finished`, or
`provider.cache_miss_diagnostic`. Valid reports derive these facts with explicit harness
source. The canonical response-finished fact remains durable, immutable, and must-pass;
the other canonical facts retain their existing interception and persistence policy.

Submitted and updated reports require exact pending-prompt ownership and a non-canceled
prompt. The harness derives an update's agent id from prompt state, enriches compaction
metadata, records semantic output, updates watcher retry state, and publishes only
updates with public content. Provider-owned deltas, status, and content-free response
stats otherwise pass through unchanged.

Cache diagnostics require exact prompt ownership but intentionally do not check
cancellation. Cancellation retains the provider route until terminal cleanup, so an
owned diagnostic may remain valid during that interval. A report after route closure
remains only an observation.

Retry results use independent pending request correlation: exact request id, provider
source, and prompt id. A valid result consumes the request once and sends a
harness-sourced `ui.retry_prompt_result` only to the captured requester. Prompt terminal
closure does not clear this correlation; existing provider disconnect, requester
disconnect, and session rollover cleanup does. There is no canonical
`provider.retry_prompt_result` event.

## Terminal response behavior

A current-owner finished report enters the existing response terminal pipeline. The
harness discards provider claims to recovery disposition and context-limit telemetry,
then derives agent identity, usage, telemetry, recovery, normalization, transcript,
watch, tool, side-conversation, prompt-route, and turn effects from harness state.

Canceled, stale, unknown, and duplicate reports produce no canonical response.
Standalone-compaction success derives `agent.compacted`; invalid standalone compaction
derives its failure and emits provider-finished only in the existing telemetry-bearing
case. Ordinary and reactive-recovery paths retain their canonical response behavior.
Every report-derived alternative uses harness source.

Report and successor publication are intentionally not transactional. The report commits
first. Canonical interception may park, and required canonical persistence may fail,
without rolling back terminal state, recovery, tool dispatch, side completion, or turn
closure already performed by the terminal pipeline. A failed canonical append still
prevents that canonical event's broadcast and downstream reaction. Fallible terminal
dispatch errors propagate through the harness pending-publish error boundary.

Raw terminal report delivery and debug records clear embedded provider-image bytes.
Ephemeral-agent debug suppression covers inbound and committed reports and canonical
updates/responses without making reports persistence targets.

This implements the Provider execution row of
[DECISION-generic-peer-event-emission](DECISION-generic-peer-event-emission.md).
Streaming payload semantics are refined by
[SPEC-provider-response-streaming](SPEC-provider-response-streaming.md), watcher
projection by [SPEC-agent-watch](SPEC-agent-watch.md), and recovery by
[SPEC-compaction-and-context-recovery](SPEC-compaction-and-context-recovery.md).
