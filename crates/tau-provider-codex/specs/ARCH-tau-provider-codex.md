# ARCH-tau-provider-codex: tau-provider-codex architecture

Provider output is constrained by
[SPEC-provider-response-streaming](../../../specs/SPEC-provider-response-streaming.md).
Curated provider wire evidence follows
[SPEC-tau-provider-codex-curated-vcr](SPEC-tau-provider-codex-curated-vcr.md).

## Account quota telemetry

This adapter owns the isolated ChatGPT `/wham/usage` full-snapshot and WebSocket
`codex.rate_limits` sparse-observation contracts. It normalizes only bounded pool/window
facts and preserves independent usage and timing observations; credentials,
account ids, credits, and provider prose never leave the in-process provider
boundary. Applicability and UI pacing are owned above this crate as specified
by [SPEC-provider-quota-pacing](../../../specs/SPEC-provider-quota-pacing.md).
The official WebSocket contract assigns a valid nameless `codex.rate_limits`
turn event to the canonical default `codex` pool. Explicit valid
`metered_limit_name` or legacy `limit_name` values take precedence, while a
JSON null is treated as absence. Every present non-null pool field must
normalize successfully, including a lower-precedence legacy field, or the
observation is rejected rather than falling back.

## Typed image tool output

GPT-5.6 Sol, Terra, and Luna on the ChatGPT Responses surface explicitly
publish image input and image tool-result support. Successful typed function
results lower to one `function_call_output` whose `output` array contains the
normalized `input_text` followed by `input_image` data URLs. Canonical binary
bytes remain in Tau; base64 exists only in the outgoing request. Responses Lite
omits `detail` after local high-detail preparation. Other model/routes project a
bounded omission marker and never receive bytes.

Normal inference, WebSocket, replay, and standalone compaction share this item
converter. Each request admits at most 24 MiB of canonical image bytes and 32
MiB of image-attributable data URLs. Debug request files and VCR matching
fixtures replace data URLs with metadata before persistence.

## Transport

Ordinary ChatGPT/Codex Responses inference always uses the pooled WebSocket
transport. There is no surface or transport selector and no HTTP/SSE inference
fallback. Capability, connection-limit, and retryable WebSocket failures surface
to the outer logical-prompt scheduler rather than replaying the prompt over HTTP.
HTTPS remains supported for OAuth, quota acquisition, and unary compaction.
The writer sends a 25-second `websocket_control_ping` WebSocket control frame
only to keep an idle transport path alive. It is not a Responses envelope, never
starts inference, and cannot refresh a prompt cache.

The extension supplies opaque resolved credentials and startup-stable mode/model
configuration to one `CodexRuntime`. Public backend outcomes are finite and
typed: dispatch timing, cumulative transport bytes, semantic progress, success,
cancellation, repetition, retryable failure, or terminal failure. The backend
does not expose its request, pool, quota parser, or mutable stream internals and
does not write harness events or sleep for logical retry.

All HTTP control-plane operations and the HTTP/1.1 WebSocket upgrade use the
startup-injected shared reqwest/rustls policy. WSS uses CONNECT through the
selected proxy before target TLS; plain WS uses proxy absolute-form. Both paths
disable library environment discovery and direct fallback. The same platform
verifier plus optional additive custom CA covers target and HTTPS-proxy TLS.

A fresh WebSocket path emits one fixed, content-free connecting status and owns
its same-key pool reservation through connection setup. Cancellation and
deadline behavior is specified by
[SPEC-tau-provider-codex-cancellation](SPEC-tau-provider-codex-cancellation.md).

Best-effort WebSocket prewarm uses the same shared pool and cooperative abort
seam from a provider-supervised worker, never the provider event loop. It skips
an already-reserved same-key socket. Profile and session invalidation is shared
with normal transport pool ownership.

One finite inference attempt has one immediate repair budget. A cached dead
socket, exact stale-chain code, or exact connection-limit code may consume that
budget only before semantic model output. Canonical provider codes take
precedence over generic status or prose. Every retry path preserves cumulative
received-byte accounting and reports the first request-send instant exactly
once. After semantic progress, an error is surfaced and tentative output is
cleared above the backend rather than replayed and spliced.

A successful `generate:false` prewarm is chain-eligible only on its exact socket,
profile/mode/cache identity, and lowered request fingerprint. The next request
must preserve the warmed input as an exact prefix, with only a suffix appended.
Mismatch, invalidation, cancellation, stale generation, or another owner drops
the anchor and sends full context. An initial fresh upgrade has its own 30-second
connection bound. After the first prewarm request is sent, its response wait and
any immediate repair connection/response share one absolute 30-second response
deadline. Failed work cannot publish a socket or response id.

Ordinary `previous_response_id` reuse likewise requires the current canonical
prefix through that response to exactly match the prefix represented on its live
socket. Input committed while a response was in flight can precede that response
canonically without existing in its upstream history; such a mismatch drops the
anchor and full-replays on the same socket. A successful replay publishes a new
anchor, while an exact match retains suffix-only incremental reuse.

## GPT-5.6 Responses modes

The surface choice follows
[GATE-tau-provider-codex-responses-surface-selection](GATE-tau-provider-codex-responses-surface-selection.md).
Each profile captures an explicit mode at startup. Standard mode uses top-level
instructions/tools, requests parallel tool calls, omits forced all-turn reasoning
context, preserves image detail, and carries no Lite marker. Lite compatibility
moves declarations and instructions into developer input items, requests serial
tool calls, forces all-turn reasoning context, omits image detail, and carries the
HTTP header or per-request WebSocket metadata marker. Retries, reconnect, replay,
and previous-response chaining retain the selected mode; there is no mode fallback.

Both modes suppress legacy inline `context_management` for GPT-5.6 and advertise
standalone compaction. The provider sends unary HTTP
`POST /codex/responses/compact` using the selected mode's lowering and marker, and
the harness installs its output as one replacement-window boundary. Older models
retain inline context management and ignore the profile's Lite compatibility flag.
Hosted Responses tools are not part of either contract; Tau's tools remain
client-executed definitions.

ChatGPT model metadata distinguishes the raw provider context window from the
effective window published to the harness. Standalone compaction thresholds
derive from the raw window, while UI usage and local context limits use the
provider's 95-percent effective ceiling.

The same model metadata publishes fixed-point equivalent API prices from
OpenAI's basic public pricing table. This estimate deliberately excludes tiers,
cache writes, service variants, subscriptions, and private-route accounting.

## Prompt-cache identity

First-party ChatGPT/Codex prompt-cache keys are stable per provider base URL, startup-selected Responses mode, and durable
target `AgentId`. Prompt provenance (`PromptOriginator`) is intentionally not part of
the key: a target agent must stay on the same provider cache bucket whether a turn came
from direct user input, extension-originated work, a manager relay, or an agent-to-agent
message.

The deprecated `share_user_cache_key` prompt flag is a no-op for first-party
cache-bucket selection. Any future cache-sharing behavior should be explicit
agent metadata (for example, a reviewed `share_cache_from` design) rather than
inferring cache identity from prompt provenance.

WebSocket pool keys follow the same identity as request `prompt_cache_key` values
so upstream thread/session headers and request bodies target the same cache bucket.
Both modes are labeled, intentionally causing one cold cache/socket transition
when upgrading from the former model-name-derived identity. Quota and retry
cooldown identity remain account/provider based and do not include the mode.

Provider-visible replay fidelity, sidecar validation, and typed semantic authority are
specified by
[SPEC-tau-provider-codex-streaming-replay](SPEC-tau-provider-codex-streaming-replay.md).

## Model metadata tags

ChatGPT/Codex model publication includes provider-owned capability tags such as
`shell:chatgpt` and `tools:custom-text`. These tags describe the model/backend surface;
the harness owns all policy that maps them to tool alternatives.

## WebSocket turn cancellation

The synchronous WebSocket turn loop receives transport events and cooperative
abort wakes through one inbound queue. Pool checkout and connection setup share
the same typed abort source. Exact observable behavior is specified by
[SPEC-tau-provider-codex-cancellation](SPEC-tau-provider-codex-cancellation.md).

## Tool definitions

The Responses adapter publishes and serializes both Function and Custom tool types.
Model metadata must continue to advertise both so harness prompt capability truth
matches the upstream request.

## Terminal request rejection

The Responses transport classifies canonical provider error codes before transport recovery.
In particular, `context_length_exceeded` is a typed terminal provider failure across
WebSocket events and non-2xx control-plane responses: it cannot reopen a cached
socket, replay a full request, or enter the logical-prompt retry scheduler.
Unknown stream failures and explicit transient status/code classes retain their existing
retry ownership. Terminal context classification trusts only canonical Responses
envelope `code`/`type` fields; echoed nested fields and provider prose are not
authoritative. Known canonical transient identifiers retain precedence over
deterministic HTTP status classification.

Retry classification remains provider-owned and is exported only as a closed structured
category, saturating attempt, and bounded approximate delay. The harness validates
prompt ownership before projecting those facts to watchers; provider display text and
response bodies are never watcher data.

## External provider trust boundary

This crate sends prompt/tool context to ChatGPT/Codex endpoints and parses provider
responses back into Tau stream state. Treat upstream responses and diagnostics as
crossing an external-provider trust boundary.

Explicitly enabled durable-session request and response captures serialize on
the producer and enter the shared
[`tau-provider`](../../tau-provider/specs/ARCH-tau-provider.md) bounded
process-wide FIFO
without waiting. Its detached worker performs zstd compression and filesystem
I/O; overload, worker startup/write failure, and process exit may omit captures
but cannot fail or wait for provider/UI work. The process-lifetime sender has no
shutdown, drain, or join API.

One failed finite Responses attempt submits one private schema-v1
`responses-attempt-failure` capture. The attempt ordinal is one-based per
`AgentPromptId`. Each attempt starts with zero dispatches. The pool assigns wire
index 1, then 2 after transparent repair, only immediately before it dispatches
an actual request envelope. `repair_used` records the independent repair fact,
including a replacement-upgrade failure that never dispatches a second envelope.
Request captures from that finite inference path carry the same
`logical_attempt` and exact `wire_dispatch_index`. Unary compaction captures
omit these fields rather than fabricating inference correlation.

The parser and transport boundaries construct opaque failure evidence before
the error reaches retry policy. Persistent records retain only closed
classification/transport facts, validated codes and IDs, message/reason
presence and lengths, and a bounded structural event shape. They never retain
provider prose, close reasons, raw values, headers, endpoints, proxy/account
data, request/model output, credentials, or raw library errors. This projection
is bounded and redacted but remains a private, potentially credential-bearing
artifact. Submission and fourteen-day diagnostic retention reuse the shared
best-effort writer; omission never changes provider execution.

## Streaming status boundary

ChatGPT/Codex output implements
[SPEC-tau-provider-codex-streaming-replay](SPEC-tau-provider-codex-streaming-replay.md)
under the workspace
[SPEC-provider-response-streaming](../../../specs/SPEC-provider-response-streaming.md).
Provider-authored text and tool payloads never become response-stat metadata.
Retry status may contain only the Codex-owned `RedactedProviderDetail`: a
single-line, bounded, known-secret/token-shape-scrubbed projection. It remains
potentially sensitive and is visible to the live UI. The `events.jsonl`,
watcher, and agent-message projections replace it with closed retry category,
attempt, and delay fields and never receive this detail.

## Transient reply hints

Message-envelope `reply` attributes are capability hints, not durable authority.
Responses chaining must not preserve an older server-side rendering after route or
effective-tool liveness changes.
