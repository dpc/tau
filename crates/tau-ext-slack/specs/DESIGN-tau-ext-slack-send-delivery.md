# DESIGN-tau-ext-slack-send-delivery: Bounded cancellable Slack send delivery

Status: confirmed, 2026-07-15, dpc

`slack_send` uses process/session-scoped at-least-once delivery. After validating
one exact tool intent, the extension freezes its agent, arguments, canonical
reply or configured destination, session/config/capability generations, frozen
credential-bearing configuration, optional preflight bot observation, scoped
tool lease name, final logical text, and exact serialized `chat.postMessage`
body. It reserves the `ToolCallId` before I/O and runs both
initial HTTP work and retry waiting outside tau-client's serialized reader.

The delivery budget is one initial attempt plus at most one retry. Retry reuses
the byte-identical route and body. A parsed numeric `Retry-After` is clamped to
1–60 seconds; otherwise retry waits at least one second plus bounded
deterministic jitter. Each native channel has a live logical-call FIFO: the front
call owns the turn through provider I/O and its possible retry, and each actual
attempt start advances the channel's one-second pacing barrier. Unrelated
channels and lifecycle/control messages remain independent. Waits are
notification-driven and cancellable; a notification causes exact authority and
remaining-time revalidation rather than cancellation by itself.
The retry must begin within 60 seconds of intent preparation; a Retry-After or
per-channel deadline beyond that horizon terminalizes without violating Slack's
requested delay. The absolute horizon is checked again atomically with the retry
state transition immediately before provider I/O.

Lifecycle, capability, route, configuration/credential snapshot, session, agent,
and reservation authority are revalidated immediately before each attempt and
again before accepting its result. The authorized `ToolStarted` grants the
logical call's scoped-tool lease; the harness revalidates that tool when
accepting completion because protocol v11 has no separate mid-call role-revocation
generation. Unregister, unload, capability loss, configuration replacement,
session replacement/shutdown, disconnect/EOF, or process shutdown cancels
provider/retry authority and prevents stale private reaction activation. Once
Slack acceptance has created `AwaitingCompletion`, ordinary agent
unload/unregister keeps that Tau completion correlation through its durable
accept/reject decision; session/process retirement clears the whole horizon.
Already-started synchronous HTTP cannot be forcibly recalled; a late accepted
effect may therefore remain remotely visible even when retirement correctly
discards its stale completion.

Delivery HTTP threads are process-owned and detached from the protocol `run`
return path. Disconnect/EOF synchronously retires their authority before reader
cleanup, but `run` does not join an already-started blocking HTTP call; it may
remain alive for the configured 30-second request timeout and may still create
the irreducible remote effect above. It cannot start a retry or restore local
authority afterward, and process exit ends all such workers.

After Slack acceptance, `AwaitingCompletion` retains the exact Tau completion.
The delivery/output worker uses tau-client's acknowledged write-and-flush path,
never the protocol reader. Writer failure retires all outbound authority, wakes
queued/retry workers, and requests shutdown. A Tau-accepted completion becomes a
stable `Completed` tool result even if reaction ownership is no longer current;
only private reaction installation depends on current local authority. Completion
resubmission shares the same 64-worker bound as provider delivery and coalesces
per call id. A bounded pending-completion FIFO drains when any shared slot frees,
so saturation never silently abandons a replay. Agent unload/unregister retains
correlation until Tau accepts or rejects it.

## Replay ledger

The process/session ledger has 1,024 non-evicting entries. A new call is rejected
before configuration freeze or Slack I/O when full. Each entry retains the exact
intent and frozen authority plus an explicit in-flight attempt, retry-scheduled
state, awaiting-Tau completion, durably completed result, definitive failure,
exhausted-unknown outcome, or cancellation. Identical same-`ToolCallId`
delivery coalesces while active and later replays the stable completion/result
or error without Slack I/O.
Conflicting agent/argument reuse is rejected. A new `ToolCallId` is new intent.

Ordinary unregister, unload, or capability churn cannot erase the accepted
intent. The ledger clears only when the harness retires the session/process
horizon (or before any accepted effect during replaceable pre-freeze
configuration).

## Ambiguity and copy count

Timeout, transport interruption, 5xx/service failures, and malformed
post-I/O responses may mean Slack accepted the request. Tau deliberately retries
once because omission is worse than a rare duplicate for this notification
surface. If the initial outcome was ambiguous and the retry succeeds, one or two
Slack copies may exist. If both outcomes are ambiguous, zero, one, or two copies
may exist. A definitive retry cannot erase the possible copy from an ambiguous
initial attempt. Definitive auth,
permission, target, or request rejection is not retried.

This is neither exactly-once nor durable restart-spanning idempotency. Process
loss clears the ledger, so replay after restart may post again. Tau does not send
`client_msg_id`, maintain a durable outbox, or reconcile remote Slack history.
Mandatory typed workspace/team installation evidence remains the subsequent
canonical Slack integration stage; an optional preflight bot observation is not
installation proof.

## Content and diagnostic boundary

Agent-authored mrkdwn rejects raw `<@`, `<!`, and `<#` native controls. The only
mention seam prepends one exact internally generated U/W source mention; agent
text is validated before that decoration. Bridge-owned help, agents, select,
to, error, and capacity messages use escaped, component-bounded
`BridgeLiteral` with `mrkdwn:false`, `link_names:false`, and a final scalar/byte
cap.

HTTP, identity, and post failures cross the provider boundary only as closed
typed categories and bounded Retry-After. Raw bodies, Slack error strings,
headers, transport errors, tokens, native ids, mentions, and message text never
enter `Display`, tool errors, notices, or ordinary logs.

This refines
[ARCH-tau-ext-slack](ARCH-tau-ext-slack.md) and the proactive authority in
[DESIGN-tau-ext-slack-proactive-sends](DESIGN-tau-ext-slack-proactive-sends.md).
