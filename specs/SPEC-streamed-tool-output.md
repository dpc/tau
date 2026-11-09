# SPEC-streamed-tool-output: Ordered tool source and completion receipts

## Record justification

This contract spans producer batching, configured-report admission, canonical
persistence, context folding, scheduling, waits, recovery lookup, and generic UI
presentation; no single implementation area can own it.

## Source and completion

Tool text formatting belongs to
[SPEC-tau-proto-provider-data](../crates/tau-proto/specs/SPEC-tau-proto-provider-data.md).
Producers submit ordinary header/payload tool text. When the harness injects it
into generic user-role context, it supplies the registered envelope, as governed by
[SPEC-exact-sentinel-prompt-envelopes](SPEC-exact-sentinel-prompt-envelopes.md).

Tools opt into ordered text output with the `tool:output-stream` declaration tag.
The harness freezes the policy from the effective prompt tool snapshot and
records the exact opted-in output-item indices on the canonical provider
declaration. Registry changes, silence, admission failure, and restart do not
change that declaration's policy. Image-capable tool declarations retain their
ordinary native foreground image closure rather than entering this text lane.

Configured Tool, Core, and Provider extensions may report incremental text only
for their own current routed invocation and connection generation. Provider
authority covers its own tools, not general shell authority. The transient
`tool.output_reported` report crosses ordinary interception and commit before
validation; peers cannot author canonical `tool.output_updated` facts.
Canonical updates identify the exact declaration occurrence and item index,
not just a reusable display call ID. They are immutable and must-pass.

An invocation's accepted updates have contiguous zero-based sequence numbers.
Identical retries have no second effect; gaps, conflicting duplicates,
empty heartbeats, foreign reports, and late reports do not become source facts.
Accepted canonical updates retain order through parked publication, semantic
admission retry, deferred native rounds, and deferred inference context.
Completion does not supersede or discard accepted updates. The harness holds its
terminal behind them. The terminal owns the last source occurrence; it is not
another update event.

The harness delivers every accepted source occurrence once. A foreground terminal
folds its own deferred typed batches and separately bounded final capture into
one native tool result, preserving the real success, error, or cancellation
status. It emits neither a completion receipt nor a duplicate source envelope.
Background reservation, not elapsed time or the number of batches, chooses the
background lane; a reserved placeholder wins even before it commits. A selected
foreground terminal cannot background while waiting behind accepted updates.
Source already assigned to the background lane never moves back to native output.
Historical committed stream-receipt terminals keep their original projection.

Background source and historical stream-receipt source use the registered
`tool_output` generic user-role envelope. Its ordered attributes identify
call and terminal status, followed by `omitted_bytes` only when nonzero.
New terminal captures expose logical `status="completed"`, `"error"`, or
`"canceled"`; incremental updates omit status. This presentation spelling does
not change durable `cancelled` outcomes or wait receipts. Available cancellation
output remains payload, including genuinely empty output; unavailable capture
retains its useful diagnostic rather than pretending to be empty.
Tool identity and sequence remain in canonical metadata and validation, not the
model-facing header. The envelope body contains untrusted tool data. Final source
precedes background completion settlement. Historical stream receipts remain
successful synchronization results whose `outcome` identifies the terminal status.
Non-opted foreground tools retain native output, including images.

All background tools place their final bounded source at the canonical terminal,
independently of a later wait or notification. Completion notices carry only
control and correlation. Successful waits carry `cause` (the source call ID) and
`outcome`, without source payload. A target error
or cancellation does not make synchronization itself fail. Plural waits keep
request order and existing atomic consumption, ownership, generation, and
rollback rules. Invalid waits still fail normally.

Background placeholders are typed tool responses with `status: backgrounded`
and any existing delegation metadata. `cancel` returns `status: ok` only to
acknowledge acceptance of the request, not terminal completion. The injected
`tool_completion` notice retains its existing scheduling role without duplicating
source or telling the model to fetch output. Text grammar belongs to
[SPEC-tau-proto-provider-data](../crates/tau-proto/specs/SPEC-tau-proto-provider-data.md);
semantic carrier framing belongs to
[SPEC-exact-sentinel-prompt-envelopes](SPEC-exact-sentinel-prompt-envelopes.md).

The canonical background-error terminal records error or cancellation as a typed
outcome, separate from its causal observation. A routed producer's accepted
cancellation remains cancelled even if the harness observed no cancellation
request; its cause may remain `Unknown`. Live and restored waits read that
outcome rather than infer it from an error message.

## Scheduling and suppression

Incremental source is ordinary activating tool-origin input. It can wake an idle
agent or interrupt an eligible wait without consuming the source completion.
It never cancels active provider inference. Existing background policy and
foreground continuation barriers remain in force; opting in does not force a
tool into the background or bypass `BackgroundSupport::Never`.

When native foreground completion absorbs deferred updates, native continuation
fulfills their exact occurrence wakes before the aggregate materializes, including
on cold replay before another dispatch. Other calls' deferred inputs and background
wake obligations retain their order and authority.

`wait.complete_only` defaults to false and applies to exact, plural, bare, and
timed-input waits. While the wait owns its registration or retained settlement,
it masks nonterminal tool-output wake causes for that agent, including causes
already queued before registration. Capture and ordered context placement
continue. Human input, messages, timers, and actual terminal notification causes
remain eligible. Retiring the wait retires its mask.

Final source is nonactivating. Existing foreground continuation,
background disposition, correlated completion notice, and dedicated
self-compaction ownership remain responsible for terminal activation. No
synthetic final-source replay wake duplicates that authority.

## Bounded custody and recovery

Incremental payload is limited to 8 KiB per update, 64 updates and 64 KiB per
invocation. A separate 8 KiB final rendering retains a bounded prefix and recent
diagnostics with explicit omitted-rendered-byte accounting. Streamed canonical
terminals carry that typed capture rather than retaining the full producer
payload beside it. Small coherent process-outcome metadata remains available to
trace consumers; oversized display metadata is replaced with a bounded generic
text display. Wait custody retains completion metadata, not source captures.

The harness admits at most eight captured invocations per agent and 64 globally
before execution effects. A slot remains occupied until completion and all
accepted source occurrences materialize, even if another foreground sibling
keeps their context deferred. Absorbed native occurrences resolve to their shared
result aggregate only when that aggregate materializes. The final allowance is
not reapplied to accepted incremental bytes during composition. Owner teardown
releases custody. Cold restore
blocks new captured invocations while genuinely deferred source or unresolved
restorable calls remain; it does not reconstruct processes or mint capture slots
for historical off-branch background placeholders.

These limits govern captured source and queued source context, not all harness
memory. Pre-effect capacity rejections have fixed, tiny final/receipt control
metadata and no process or capture slot. Their cardinality is separately bounded
by the existing finite canonical declaration and one-open-round rules. Persistence
staging applies finite frame/byte limits and reserves separate capacity for
terminal/control work rather than letting incremental output consume it all.

There is no dedicated output lookup tool. Use ordinary shell commands to inspect
saved output through the terminal's temporary-file path, when available. Use
existing best-effort compaction transcript archives to inspect accepted source
in replaced semantic windows, as described in
[SPEC-compaction-and-context-recovery](SPEC-compaction-and-context-recovery.md).
Neither route recovers omitted raw bytes or expired temporary files.

Normal semantic journal/replay rules apply. Accepted source survives replay;
unaccepted reports, omitted raw bytes, and temporary saved-output files have no
stronger durability promise. Restart never reruns a command to reconstruct
output. Genuine deferred incremental input follows existing queued-input
recovery, including one supersession of a dispatch-uncertain owner. Final-only
source does not supply that authority. This change does not close the existing
background-terminal-before-completion-notice crash gap or add fsync guarantees.

## Shell cadence

`shell` and `shell_command` flush pending text after 15 seconds without new
output or 30 seconds since the oldest pending output, whichever comes first.
Each batch resets those clocks. Silence produces no nonterminal update; reaching
a size limit never causes an early flush. Partial lines and split UTF-8 are
handled without inventing line boundaries. Actual completion flushes the final
tail immediately, even when it is empty.

Available cancellation tails follow the same final-source path without
repeating earlier accepted batches. An available tail may be empty; an absent
tail explicitly means unavailable.

When publisher capacity is exhausted, the producer retains one bounded,
immutable sealed batch and continues draining pipes into a bounded recent tail.
A bounded capacity registration made before effects wakes the producer when
admission can be retried. No fixed timer triggers retries, and ordinary pipe
activity does not repeatedly retry a sealed batch. After process draining ends,
final publication uses a checked, acknowledged send ordered behind accepted
detached reports.

The producer continues draining output and executing the command when output
exceeds an allowance. It preserves recent diagnostics and reports omissions
rather than cancelling the process. A private saved-output spool has an
independent finite cap and truthful complete, truncated, or unavailable metadata;
it is not durable storage. Shell VCR replay preserves accepted batch order and
relative timing without executing the command.

This refines
[SPEC-terminal-tool-reports-and-canonical-outcomes](SPEC-terminal-tool-reports-and-canonical-outcomes.md)
and
[SPEC-exact-sentinel-prompt-envelopes](SPEC-exact-sentinel-prompt-envelopes.md).
