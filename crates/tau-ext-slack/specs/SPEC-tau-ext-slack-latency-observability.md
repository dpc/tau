# SPEC-tau-ext-slack-latency-observability: Slack latency telemetry

At most 64 Slack occurrences may be reserved, queued, or active. Reservation
happens before ACK and commits only after the local ACK flush. Saturation or
closure reconnects without ACK. Successfully ACKed work remains FIFO across
reconnect, but is memory-only and has no post-ACK durability. Blocking identity,
local post, and admission work runs only on the serial worker.

Lifecycle generation invalidates late or gap work; reconnect alone does not.
Sender verification is live with no positive cache. `slack_latency_v1` is
TRACE-only monotonic microseconds with bounded event/outcome/depth values and
process-local ordinals. ACK means local flush; queue wait starts after handoff.
Telemetry never includes payload/frame/body, identity, route, message, event,
alias, timestamp, URL, token, agent ID, or stable hash; it is never durable.
Retention is bounded and ordinals are never metric labels.

The closed stage set is frame receipt, envelope decode, pre-ACK reservation, ACK
attempt, ACK completion, FIFO dequeue, identity start, identity completion,
local-post start, local-post completion, and direct fact publication. The closed
ordinal classes are connection, occurrence, and request.
