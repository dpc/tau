# SPEC-provider-prompt-materialization-authority: Provider prompt materialization authority

## Record justification

Prompt materialization spans core journal folding, harness dispatch and
interception, provider routing, protocol projection, recovery, and diagnostics,
so no one local artifact can own its authority and crash-boundary contract.

Canonical transcript and compaction replacement facts, not a historical full
provider request, remain provider-content authority. An inference dispatch
checkpoint or standalone-compaction start is the sole durable recovery and
no-resend owner; its terminal outcome is completion authority.

Tau-owned local compaction summaries retain a typed
`SyntheticCompactionSummary` origin in their canonical replacement message.
Prompt assembly uses only that origin, never the narrative spelling, to select
synthetic-summary provenance guidance. Provider materialization still emits the
accepted narrative bytes unchanged under the existing user role. A complete
canonical reserved Tau provenance envelope is rejected before the replacement
fact commits; substrings and lexical near-matches do not trigger rejection.

Harness-authored `agent.prompt_started` is the content-free durable fact that one
full request was materialized and admitted for dispatch. It must match exactly
one unresolved owner by agent, prompt, model, and operation. The content-bearing
`agent.prompt_created` remains a transient point-to-point work envelope available
to its selected provider and existing live observers; it never enters semantic
storage.

The owner must commit before materialization. The prompt-start fact must then
complete bounded persistence admission and its in-memory fold before provider
delivery, and its post-commit
continuation exclusively owns that delivery. Immediately before send, the
continuation revalidates the same session and loaded runtime incarnation, the
unresolved owner, the unique prompt-start fact, their matching identity fields,
and the current model route. Each owner and `(agent_id, agent_prompt_id)` admits
at most one prompt-start fact and one live continuation. Delivery consumes the
continuation once; replay never recreates it.

Recovery never reconstructs or resends the transient full request, including
after a crash between owner commit, prompt-start commit, and provider delivery.
A journal may end after the owner and before prompt-start. Duplicate prompt-start
facts are invalid. Persisted full `agent.prompt_created` records are unsupported;
Tau provides no migration, backfill, dual-read, or mixed-format precedence.

An output-length continuation follows the same owner and prompt-start authority.
Its embedded response plan may repair only the missing internal steer and missing
owner. Once the owner exists, the ordinary conservative boundary applies:
missing prompt-start or missing provider terminal is dispatch-uncertain and Tau
never reconstructs or resends the request.

Historical subscriber catch-up excludes prompt-start facts. Best-effort debug
output contains only a bounded content-free summary. Optional exact request
capture is diagnostic output with explicit bounded retention and is never
semantic authority, replay input, or recovery state.

The live-acceptance versus worker-persistence crash boundary follows
[SPEC-semantic-journal-writeback-durability](SPEC-semantic-journal-writeback-durability.md).
