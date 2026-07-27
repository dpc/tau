# DECISION-tau-ext-xmpp-tool-delivery-lifecycle: Bounded, cancellable XMPP tool delivery

Authority: confirmed, 2026-07-22, dpc

## Status

This is the confirmed prospective end state. This record documents the design
only and does not authorize implementation. The current extension still performs
readiness, registration, sequential multipart send, and unregister waits on
`tau-client`'s serialized reader with resetting per-step and command bounds.
The executor, FIFO, whole-intent deadline, generation revocation, and exact
terminal accounting described here do not yet apply. Current behavior remains
described by [ARCH-tau-ext-xmpp](ARCH-tau-ext-xmpp.md),
[SPEC-tau-ext-xmpp-readiness-waits](SPEC-tau-ext-xmpp-readiness-waits.md), and
[SPEC-tau-ext-xmpp-muc-lifecycle](SPEC-tau-ext-xmpp-muc-lifecycle.md).

XMPP tool delivery will use one XMPP-local strict FIFO shared by every agent and
by register, send, and remote-unregister work. Capacity is 32 live records,
including the executing record. Successful reservation order is execution
order; multipart sends do not interleave. One frozen logical body is retained,
with at most one UTF-8-safe part rendered at a time.

Every admitted register or send intent has one absolute monotonic deadline 60
seconds after reservation. Queue time counts and the deadline never resets.
Existing readiness, registration, stanza, and worker-command caps remain as
smaller inner bounds clamped to the remaining whole-intent time.

Local lifecycle authority is independent of fallible remote cleanup.
Configuration invalidation, unload, session rollover or shutdown, explicit
unregister, `Disconnect`, and output loss synchronously revoke the applicable
process-local generations and routes. Revocation cancels queued work, signals
active work, and prevents later effects or publication. Remote cleanup joins the
FIFO tail on a best-effort basis and never delays local revocation.

Each admitted call has one terminal owner and at most one attempted terminal
report. Cancellation is definitive only before a remote effect starts. After a
stanza is handed to the transport, failure, timeout, or revocation reports the
bounded ambiguity that zero or one copy of the current stanza may exist; earlier
accepted parts remain visible. There is no automatic retry and no exactly-once
remote-delivery claim.

Successful sends retain report-before-result ordering:
`message.sent_reported` with the original logical body is attempted before
`tool.result_reported`. Immediate failure of the report suppresses the result.
Detached output admission remains non-transactional and does not acknowledge
encoding, flush, harness observation, canonical fact acceptance, XMPP delivery,
or read receipt.

The exact lifecycle, outcomes, and publication contract is
[SPEC-tau-ext-xmpp-tool-delivery-lifecycle](SPEC-tau-ext-xmpp-tool-delivery-lifecycle.md).
This choice is governed by
[GATE-persistence-and-extension-interface-change-approval](../../../specs/GATE-persistence-and-extension-interface-change-approval.md).

## Rationale and tradeoffs

Capacity 32 absorbs a modest burst while bounding retained logical bodies to
about 4 MiB plus metadata and one rendered stanza. The absolute 60-second
deadline bounds multipart head-of-line occupancy. Strict FIFO preserves serial
remote behavior, while generation leases make local revocation prompt even when
remote cleanup fails.

The design deliberately accepts head-of-line blocking, dropped best-effort
cleanup when the FIFO is full, non-transactional multipart visibility, one
bounded ambiguous current stanza, and report-without-result output failure
windows. Smaller payloads or resetting per-step timeouts would not solve
authority revocation. The decision does not change generic `tau-client`
concurrency or its detached-writer contract.
