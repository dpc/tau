# SPEC-durable-tool-observation-correlation: Explicit tool and wait causality

## Record justification

This contract spans persisted protocol envelopes and facts, harness runtime
emission and replay, and offline compact projection, so no one implementation
area can own it coherently.

## Contract

Every persisted agent occurrence carries one opaque random 128-bit
`ObservationId`. A `ToolCallRef` identifies one provider-declared call by its
declaration occurrence and zero-based output-item index. Neither identity encodes
time, order, an agent, or a journal sequence.

The content-free observation events are
`agent.tool_dispatch_observed`, `agent.tool_backgrounded_observed`,
`agent.tool_wait_observed`, `agent.tool_wait_registered`, `agent.activation_queued`,
`agent.tool_wait_settled`, `agent.tool_cancellation_requested`, and
`agent.tool_terminal_classified`.

Every provider-declared wait receives a pre-resolution
`agent.tool_wait_observed` identity. Its typed mode distinguishes a resolved
exact target, a bounded ordered resolved exact-target set, unresolved exact
selection, next-background selection, activating-input timeout, and invalid
arguments. A plural selection contains one through 64 distinct targets in request
order and records no synthetic target when any requested display ID cannot resolve
to its exact declaration occurrence. Active registrations and every settlement
refer back to that observation; immediate settlements retain no registration.

They are valid per-agent journal records but replay as fold no-ops. Replay never
dispatches work, installs or settles a waiter, queues input, consumes output,
cancels a call, or repeats a continuation.

A canonical final terminal owns its output. Its event-envelope observation ID is
the output reference. A completion-delivering wait retains only that reference and
its typed envelope; it never owns or copies the source payload or output counts.
A successful plural wait retains one bounded ordered delivered-source record per
requested target. Every source record contains the exact call reference, canonical
terminal observation, terminal phase, and wait envelope. All records share the
plural wait call and terminal, preserve request order rather than completion order,
and copy no source payload.
A terminal classification precedes the canonical terminal, and a wait settlement
can survive only after that canonical terminal commits.
An undelivered background-completion notice retains the same exact call and
terminal pair. If a provider-visible call ID is reused, each generation keeps a
distinct notice identity; the harness neither deduplicates nor removes one
generation's notice through another generation's display ID.
When UI manual compaction preempts the sole installed harness-owned wait, its
canonical null-output cancellation produces
`agent.tool_wait_settled` with outcome `Cancelled`. Because no
provider-declared cancel call exists, the harness emits no
`agent.tool_cancellation_requested`; the terminal classification cause remains
`Unknown`.

Wait registration, settlement, activation, cancellation, and terminal causes use
typed references. Missing crash-tail or selected-cut endpoints stay explicit as
`source_not_selected`, `unresolved`, or `incomplete`. Consumers must not infer an
edge from call-ID text, timestamps, prose, or adjacency. Qualified elapsed
intervals require both explicitly linked endpoints in the same agent journal and a
nondecreasing producer wall clock.

Selected endpoints in another agent journal use the same non-fatal
`source_not_selected` or `unresolved` fallback as unavailable endpoints. They
never transfer terminal status or output ownership and never produce a
cross-journal interval. Normal runtime teardown does not transfer background-call
ownership; this fallback instead keeps selected subsets and incomplete or
historical journals non-fatal.

Configured extensions may classify
`ExtInternalPromptSubmitRequest.activation_kind` as `timer`. Absence and explicit
`internal_prompt` both mean an ordinary internal prompt. The transient request
does not become semantic history; its accepted classification is copied into the
content-free activation observation. Other activation kinds remain harness-owned.

Observation appends validate and write a failure-atomic frame synchronously, but
their failure never changes the observed runtime action or result. Stable-storage
sync remains asynchronous under
[`GATE-asynchronous-journal-durability`](GATE-asynchronous-journal-durability.md).


## Projection

The `tau.agent_trace_compact` schema version `0` emits `call`, `activation`, and
`relationship` items alongside assistant/reasoning and explicit directional
message items. JSON Lines and strict TOON carry the same semantic item set.
Every item carries relative append time, optional absolute append time, owning
journal identity and sequence, plus provider item position when applicable.
Presentation sorting uses the exact lexicographic tuple `(recorded_at, agent_id,
journal_seq, item_index-or-0, family rank)`, where an absent provider item index
sorts as zero. It creates no causal authority and does not replace
journal-sequence order. Lite mode bounds each semantic text and source-owned
terminal output to 4 KiB while retaining exact complete byte and line counts.
Full mode retains complete content. The family rank order is `call`,
`assistant_message`, `assistant_reasoning`, `message_sent`, `message_received`,
`activation`, then `relationship`.

Resolved `shell`, `shell_command`, and `gpt_shell` calls may additionally carry
bounded `shell_outcome` metadata read only from the canonical terminal's raw
structured result or error details. Its process success is independent of the
call lifecycle `status`; malformed, unavailable, cancelled, or unresolved
outcomes remain absent. Foreground and background terminals, JSON Lines and
TOON, and lite and full modes use the same outcome semantics.
The exact source mapping, accepted per-reason field matrix, legacy
result-payload fallback, and malformed-data rejection contract are defined by
the compact trace interface in
[`docs/agent-trace.md`](../docs/agent-trace.md#compact-semantic-traces).

The native `tau.agent_trace` JSON Lines occurrence includes its observation ID,
journal identity, sequence, timestamp, source, parent, and lossless typed event.
