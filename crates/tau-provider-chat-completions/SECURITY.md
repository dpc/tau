# Chat Completions transport boundary

Chat Completions sends prompts, tool definitions, credentials, and conversation
context to configured external endpoints. Every attempt receives the built-in
provider process's immutable `OutboundNetworkPolicy`; reqwest environment proxy
discovery and redirects remain disabled. A selected proxy is the only route
for that attempt and failure never falls back direct.

Target and HTTPS-proxy TLS use platform verification plus the optional
startup-read custom CA. Plain HTTP endpoints expose headers and content to a
selected proxy. Cancellation drops the active async response future/socket;
header, body, and idle waits retain finite bounds. Error bodies remain bounded
and provider content must never enter retry status, ordinary logs, or watcher
events.

Explicit durable-session debug captures can contain complete prompts, tool
results, model output, and bounded HTTP error bodies. They remain private
sensitive `.json.zst` diagnostics; compression is not redaction. The shared
bounded writer can omit them on overload, failure, or process exit and never
blocks provider work to guarantee persistence.
Tau does not intentionally include auth headers or API-key configuration, but
provider-controlled responses/errors and configured request fields can reflect
credentials, so every capture must be treated as potentially credential-bearing.

## Local summary compactor boundary

Only a model with an explicit `local_summary_compaction` profile may run Tau's
transcript-v1 summary compactor. The dedicated exact-model request has no tools,
omits image bytes while retaining typed metadata and an explicit loss marker,
and is excluded from durable provider debug capture. Only a bounded, validated
six-section result persists, as untrusted synthetic user-role history. Failures
after semantic output terminalize without resend. Revisit this boundary when
changing compactor serialization, capture, retries, validation, or eligibility.
