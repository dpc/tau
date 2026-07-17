# DESIGN-tau-ext-slack-latency-observability: Bounded serial admission and payload-free timing

Status: confirmed, 2026-07-14, dpc

Slack Socket Mode uses one process-local FIFO with a hard bound of 64 supported
occurrences across pre-ACK reservations, queued work, and the one in-flight worker.
The reader reserves before ACK, commits the reserved slot only after successful
local ACK write, and reconnects without ACK on saturation or worker closure.
Successful-ACK order is therefore the sole serial processing order across
reconnects. The handoff is memory-only and adds no post-ACK durability claim.

Blocking live `users.info`, bridge-local `chat.postMessage`, and admission work run
only on the serial worker. Reconnect does not invalidate accepted work. Inactive
configuration replacement, session start/shutdown, and process shutdown advance
explicit lifecycle authority; late I/O results and gap occurrences cannot create
effects or ingress in another authority generation. Sender verification remains
live per occurrence and fail-closed; no identity cache is part of this decision.

The private `slack_latency_v1` TRACE schema uses monotonic microsecond durations,
bounded event/outcome/depth classes and extension-process-local
connection/occurrence/request ordinals. It never logs
raw frames, text, response bodies, Slack identities, channels, threads, messages,
events, envelopes, aliases, remote timestamps, URLs, tokens, agent identities, or
stable hashes, and never enters durable events. Ordinals disclose local
volume/order, are approved for TRACE correlation only, require bounded local log
retention, and must never become metric labels.

The stages are websocket frame receipt, envelope decode, pre-ACK reservation,
ACK attempt/completion, FIFO dequeue, identity start/completion, post
start/completion, and direct fact publication. “ACK written” means local
websocket flush only. Queue wait starts after successful ACK handoff. Correlation
uses only the Slack extension's process-local ordinals; arbitrary external data
is never logged.
