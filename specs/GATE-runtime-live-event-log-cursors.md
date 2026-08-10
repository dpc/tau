# GATE-runtime-live-event-log-cursors: Preserve cursor-followed live event delivery

## Gate

Tau must maintain one logical process-local live event log with monotonically
ordered runtime-only positions. Runtime positions must not become persisted or
wire authority.

Each component connection's egress task must follow that shared live log from
its own subscription cursor and deliver at its own pace. Live delivery must
retain one shared in-memory event representation only while a currently
eligible consumer generation can still require it, and must prune the
representation after all such generations advance or cease to qualify. A
subscription or catch-up must establish a live-stream barrier: the consumer is
eligible only for the applicable live suffix, and a late subscriber must not
cause earlier transient events to be retained. Per-component unbounded event
egress queues must not replace this architecture.

Component ingress must cross a trivially small bounded or rendezvous channel
and be naturally backpressured by harness consumption. Correctness and
lifecycle completion must hold at channel capacities zero and one; larger
capacity is performance tuning, not correctness authority.

A connected consumer remains eligible while stalled and may pin shared
in-memory retention indefinitely. Tau must not globally backpressure or reject
publication because of egress lag, spool the live stream to disk, or
expire/disconnect a consumer solely because of lag. A rate-limited lag warning
may report pathological delay but must not alter delivery or lifecycle.

This gate does not establish a persistence, replay-storage, or wire contract.

## Justification

The user wants one shared runtime ordering and retention authority, independent
consumer pacing, prompt reclamation after all applicable delivery cursors
advance, and end-to-end component-ingress backpressure. The user specifically
does not want unbounded per-component egress queues or larger ingress queues to
return as implementation conveniences, and has chosen faithful retention over
an implicit lag-disconnection or publication-backpressure policy.
