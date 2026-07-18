# DECISION-provider-response-stats: Provider-owned response statistics

Authority: confirmed, 2026-07-08, user

Provider transports own response-byte sampling and publication because they own
backend dispatch and read the upstream response stream. The harness only
validates prompt ownership/cancellation, derives public routing identity from
prompt ownership, and passes accepted content-free samples through unchanged; it
does not count, resample, strip, project, or fold them into transcripts. UI
clients render the provider-owned samples directly from public response updates.

Centralizing accounting at the transport boundary avoids misleading counts from
semantic parsing, tool output, UI rendering, or Tau protocol framing. The
tradeoff is that providers must implement the shared cadence and cumulative
sampling contract consistently.

Exact byte scope, previous/current semantics, update cadence, terminal flush,
and consumer behavior are specified by
[SPEC-provider-response-streaming](SPEC-provider-response-streaming.md).
