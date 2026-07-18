# DECISION-tau-ext-slack-latency-observability: Bounded Slack latency observation

Authority: confirmed, 2026-07-14, dpc

Slack ingress uses bounded serialized pre-ACK admission. Observability is
payload-free local timing, depth, outcome, and process-local ordinal metadata;
it never records identities, routes, message content, native references, URLs,
tokens, stable hashes, or durable telemetry.

This makes queue delay and lifecycle loss diagnosable without widening the
external-content trust boundary. Exact capacity, stages, and exclusions are
[SPEC-tau-ext-slack-latency-observability](SPEC-tau-ext-slack-latency-observability.md).
