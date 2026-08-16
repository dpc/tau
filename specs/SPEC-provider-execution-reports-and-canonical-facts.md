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
estimated cost rates, and estimated cost increments, then derives agent identity,
usage, telemetry, recovery, normalization, transcript, watch, tool,
side-conversation, prompt-route, and turn effects from harness state. The canonical
response preserves response-local usage only when at least one provider counter
was available. Present all-zero counters remain present zero. Effective rates
and response-local cost increment are present exactly when usage is present;
otherwise all three accounting fields remain absent. The nested cumulative
usage snapshot is never durable accounting input.
The shared report shape also carries `provider_attempt`, but provider input is
untrusted. The harness overwrites it with the finite transport attempt that
produced the canonical terminal. The default is one; zero is invalid canonical
state. This counter is independent of output-length continuation ordinals.

Optional cache usage remains response-local, privacy-redacted metadata. The
harness normalizes contradictory cache classes against authoritative total input
in read, write, then miss order; independently bounds cacheable-prefix and
avoided-prefill estimates; and preserves missing observations as missing.
When nested cache usage reports a read count, that normalized count is the
harness-owned cache-read authority for the legacy cached-input counter, session
totals, cache-read ceiling validation, and cost. Without a nested read count,
the legacy cached-input counter supplies the normalized read count; a wholly
absent nested cache observation remains absent.
Effective cost uses distinct ordinary-input, cached-read, cache-write, output,
and provider-reported token-time storage classes. A missing write rate preserves
the prior ordinary-input estimate, while missing storage usage or price adds no
storage cost. Prompt content and provider cache keys never enter these facts.

Provider-reported terminal usage may also carry an optional response-local
prompt-cache-read ceiling. The harness preserves this assertion in the
canonical response only when cached input is less than or equal to the ceiling
and the ceiling is less than or equal to sent input. It replaces an invalid
assertion with absence and emits one bounded structured warning for the
accepted terminal report. The ceiling is informational: token counters,
session and model totals, estimated cost, and latency never consume it.

Canceled, stale, unknown, and duplicate reports produce no canonical response.
Standalone-compaction success derives `agent.compacted`; invalid standalone compaction
derives its failure without folding provider output into transcript history. Before
either outcome, the harness normalizes cached usage to no more than sent usage for
both live context state and the canonical report. Rejected standalone terminals retain
context/cache and transaction authority but release prompt-local cache/alert snapshots;
session token and cost accounting remains intentionally billable. Ordinary and
reactive-recovery paths retain their canonical response behavior.
Every report-derived alternative uses harness source.

`ProviderStopReason::Length` is always an incomplete semantic terminal. The
harness clears provider-authored output-length disposition and may derive one
`ContinuationPlanned` disposition only for an ordinary inference that contains
non-empty full, replay-safe reasoning and contains neither assistant messages nor
tool calls. The plan reserves one successor prompt in the same outer turn.
Assistant prose remains visible but is never stitched; a length-stopped tool call
is retained but never executed and never activates a synthetic-error follow-up.

The planned canonical response owns the continuation. Its commit completion
appends the exact harness-internal output-length steer; that steer owns the
matching inference checkpoint, and the checkpoint retains the captured model,
route-independent model identity, branch cut, and original outer turn. A second
plan in one outer turn is invalid. The successor terminal carries
`ContinuationTerminal` and an explicit `outer_turn_finish_owed` repair bit.
Each response retains separate provider usage, effective rates, and cost; totals
sum every accepted response once.
If reactive recovery claims the reserved successor, the rejected response carries
only recovery authority. The transaction-owned post-compaction inference remains
the exact successor lineage and closes it; its own `Length` is terminal because
the same-turn budget is already spent.

Report and successor publication are intentionally not transactional. The report commits
first. Output-length continuation is the narrow exception to eager successor effects:
its steer cannot be appended or dispatched until the planned response commits.
Otherwise, canonical interception may park, and required canonical persistence may fail,
without rolling back terminal state, recovery, tool dispatch, side completion, or turn
closure already performed by the terminal pipeline. A failed canonical append still
prevents that canonical event's broadcast and downstream reaction. Fallible terminal
dispatch errors propagate through the harness pending-publish error boundary.

Raw terminal report delivery and debug records clear embedded provider-image bytes.
Ephemeral-agent debug suppression covers inbound and committed reports and canonical
updates/responses without making reports persistence targets.

This implements the Provider execution row of
[SPEC-peer-event-publication](SPEC-peer-event-publication.md).
Streaming payload semantics are refined by
[SPEC-provider-response-streaming](SPEC-provider-response-streaming.md), watcher
projection by [SPEC-agent-watch](SPEC-agent-watch.md), and recovery by
[SPEC-compaction-and-context-recovery](SPEC-compaction-and-context-recovery.md).
