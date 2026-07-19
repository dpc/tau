# DECISION-provider-response-stats: Provider-owned response statistics

Authority: confirmed, 2026-07-08, user

Provider transports own response-byte sampling and publication because they own
backend dispatch and read the upstream response stream. The harness validates
prompt ownership and passes accepted content-free samples through unchanged; UI
clients render provider-owned samples directly.

Accounting at the transport boundary avoids misleading counts from semantic
parsing, tool output, UI rendering, or Tau framing. The tradeoff is that providers
must implement the shared cadence and cumulative contract consistently. Exact
behavior is specified by
[SPEC-provider-response-streaming](SPEC-provider-response-streaming.md).
