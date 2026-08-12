# SPEC-tau-ext-xmpp-tool-delivery-lifecycle: XMPP tool delivery lifecycle

## Status

This is the confirmed but mostly unimplemented end state. Implementation is not
authorized by this record. Its checked mandatory publication semantics are now
implemented under the separately approved mandatory-delivery audit fix:
accepted inbound reports fail closed, and successful sends attempt
`message.sent_reported` before their sole terminal while suppressing that
terminal after report failure. The remaining executor, queue, deadline,
generation, revocation, and observability lifecycle is prospective. Current
behavior remains described by [ARCH-tau-ext-xmpp](ARCH-tau-ext-xmpp.md),
[SPEC-tau-ext-xmpp-readiness-waits](SPEC-tau-ext-xmpp-readiness-waits.md), and
[SPEC-tau-ext-xmpp-muc-lifecycle](SPEC-tau-ext-xmpp-muc-lifecycle.md).

## Record justification

The complete lifecycle spans the protocol reader, XMPP-local executor and FIFO, worker/transport command path, registration and inbound-routing leases, extension lifecycle messages, serialized output gate, and deterministic test surfaces, so no one implementation area can describe reservation, remote effects, revocation, terminal ownership, and publication coherently.

## Admission and execution

One executor owns a strict FIFO shared across agents and register, send, and
remote-unregister work. Capacity is 32 live records total, including the record
executing; at most 31 wait behind it. Successful reservation order is execution
order. There are no per-agent lanes, priorities, retries, or part-level
interleaving.

The reader performs only bounded local validation, output-liveness checking,
reservation, and freezing. It freezes the call identity and arguments, agent,
session/config/registration generations, logical body, and fixed conversation.
A send requires an already-active registration and cannot depend on a queued
registration. The frozen call stores the logical body once and renders at most
one UTF-8-safe part at a time. The executor alone performs readiness waits and
remote register, send, and unregister effects.

Queue saturation or executor spawn failure terminalizes the call before bridge
startup or remote I/O. Local revocation is control-plane work, not queued work.
It removes and terminalizes affected queued records before appending remote
cleanup at the FIFO tail. Cleanup may be dropped if unrelated work fills the
FIFO, and that drop is observed. Revocation never waits for space or cleanup.

## Deadlines

Every successfully reserved register or send receives one monotonic deadline 60
seconds after reservation. Queue time counts. Inner waits use the lesser of
their cap and remaining total time:

| Wait or effect | Maximum |
|---|---:|
| Online readiness | 30 s |
| Executor-to-XMPP-worker command response | 60 s |
| Registration, including MUC join/configuration | 45 s |
| One stanza submission | 20 s |
| Complete multipart send, including queue/readiness/parts/publication | 60 s total |

Exhausting an inner cap terminates the intent even if total time remains. The
deadline never resets between parts, reconnects, or commands. Existing
five-second extension-shutdown and four-second worker-cleanup budgets remain
aggregate cleanup budgets outside the tool deadline and cannot postpone local
revocation.

A best-effort unregister/leave record gets a separate 60-second absolute
deadline from FIFO append and the same clamped command/stanza caps. The shorter
aggregate process-shutdown budget supersedes it. Cleanup expiry is observable
but does not change revoked authority or an unregister result.

## Authority and generations

Configuration, session, and each agent registration use monotonically increasing
process-local generations. Registration reservation also has a unique token and
remains non-routable pending remote completion. The frozen tuple is revalidated
before and after every await and before:

1. readiness or a worker command;
2. every registration action and stanza or part;
3. installing `Pending(token)` as `Active`;
4. routing an inbound stanza;
5. enqueueing `message.sent_reported`;
6. enqueueing terminal success.

Registration installation compare-and-swaps the exact pending token to active.
Session rollover/shutdown, explicit unregister, unload, `Disconnect`,
configuration invalidation before start, and output loss remove local routes and
increment relevant generations before any remote wait. Revocation signals the
active record, and no later part may start. A stale registration completion
never installs a route. Inbound routing uses the same live lease, so failed
remote cleanup cannot keep a local route alive.

## Terminal ownership and reports

A compare-and-swap selects one terminal owner among deadline, revocation, worker
completion/death, and output failure. Losers emit nothing. This gives one local
terminal disposition and at most one attempted terminal report, not exactly-once
remote delivery.

Lifecycle `ToolErrorReported` events use the frozen call ID, name, and
originator, `tool_type=function`, `details=None`, `display.status=error`, and the
same exact text for `message` and `display.status_text`. Definitive pre-effect
cancellation uses `ToolCancelledReported` with the frozen call ID/name and
`tool_type=function`. That protocol payload has no reason field; a separate
content-free structured outcome records the reason.

| Condition | Exact outcome |
|---|---|
| Queue full | `ToolErrorReported`: `XMPP operation queue is full (capacity 32); no remote I/O was attempted` |
| Executor spawn failure or worker unavailable before an effect | `ToolErrorReported`: `XMPP operation executor is unavailable; no remote I/O was attempted` |
| Deadline before an effect | `ToolErrorReported`: `XMPP operation deadline expired before remote I/O (60s total)` |
| Unload, session rollover/shutdown, explicit unregister/supersession, or `Disconnect` revokes a queued call before an effect | `ToolCancelledReported` |

Worker death drains other queued records once with the executor-unavailable
error. Unload and session shutdown attempt cancellations while output is usable.
`Disconnect` cancels before returning from the reader, but reports are best
effort because the harness may already be withdrawing the source.

## Send outcomes

Let `n` be fixed part count, `i` fully accepted parts, and `k=i+1` current part.

| Condition | Exact `ToolErrorReported.message` |
|---|---|
| Transport failure between parts | `XMPP send failed after {i}/{n} complete part(s); completed parts may remain remotely visible` |
| Revocation between parts | `XMPP send cancelled after {i}/{n} complete part(s); completed parts may remain remotely visible` |
| Deadline between parts | `XMPP send deadline expired after {i}/{n} complete part(s); completed parts may remain remotely visible` |
| Revocation after handoff | `XMPP send was cancelled during part {k}/{n} after {i} complete part(s); zero or one copy of part {k} may also exist; do not retry automatically` |
| Stanza cap after handoff | `XMPP stanza timed out during part {k}/{n} after {i} complete part(s); zero or one copy of part {k} may also exist; do not retry automatically` |
| Total deadline after handoff | `XMPP send deadline expired during part {k}/{n} after {i} complete part(s); zero or one copy of part {k} may also exist; do not retry automatically` |
| Transport/worker failure after handoff | `XMPP send worker failed during part {k}/{n} after {i} complete part(s); zero or one copy of part {k} may also exist; do not retry automatically` |
| All accepted, authority revoked before publication | `XMPP send completed {n}/{n} remote part(s), but local authority was revoked before success publication; all parts may be remotely visible; do not retry automatically` |
| All accepted, deadline before publication | `XMPP send completed {n}/{n} remote part(s), but its 60s deadline expired before success publication; all parts may be remotely visible; do not retry automatically` |

No failure or cancellation emits `message.sent_reported` or success. Successful
sends enqueue `message.sent_reported` with the original body and frozen
conversation before `ToolResultReported("sent XMPP message")`. There is no retry.

## Registration and unregister

A registration revoked before remote setup is `ToolCancelledReported`. Once
setup begins, it never installs authority after cancellation or failure and uses
one exact error:

- `XMPP registration was cancelled after remote setup began; local routing was not installed and remote cleanup is best effort`
- `XMPP registration deadline expired after remote setup began; local routing was not installed and remote cleanup is best effort`
- `XMPP registration worker failed after remote setup began; local routing was not installed and remote cleanup is best effort`

Successful registration retains:

```text
registered for XMPP messages at {address}. Plaintext over TLS only; no OMEMO/E2EE.
```

Explicit `enabled=false` first revokes authority and cancels stale work, then
returns `unregistered from XMPP messages`. This means local routing is revoked;
FIFO-ordered remote cleanup is best effort and cannot change the result. Unload,
shutdown, and `Disconnect` use the same cleanup semantics without an unregister
tool result.

## Output and observability

Immediate checked-output submission errors are not ignored. Output unavailable
before an effect terminalizes locally as `output_unavailable`, revokes all
authority, closes admission, and performs no remote I/O. Failure after an effect
does the same without retry and records remote ambiguity locally. There is no
protocol terminal error for output loss because that channel is unavailable;
the harness owns canonical fallback for pending calls.

Successful send publication uses one local gate to attempt
`message.sent_reported` and then `ToolResultReported`. Failure of the first
suppresses the second. Admission proves neither flush nor canonical commit.
Reports and facts otherwise follow
[SPEC-external-message-reports-and-facts](../../../specs/SPEC-external-message-reports-and-facts.md).

Content-free structured events cover admission, start, remote-effect start,
authority revocation, terminal disposition, and cleanup completion. They include
a process-local intent ordinal, operation kind, queue depth/capacity, generation
numbers, queue/total duration, part index/count, completed count, closed outcome,
revocation cause, and `remote_copy=none|completed_only|up_to_one_current`. They
never include bodies, complete arguments, credentials, JIDs, room/nick names, or
raw server errors.

Frozen plaintext bodies are dropped promptly after terminal disposition. This
does not claim allocator zeroization.
